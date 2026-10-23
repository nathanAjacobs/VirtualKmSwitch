//! Keeps a session agent alive on each desktop that matters, for whichever session owns the console.
//!
//! The service cannot inject input itself. It runs in session 0, which is non-interactive and has no
//! access to the interactive session's window station, so `SendInput` from here reaches nothing
//! however privileged the process is. Placing agents inside the session is the only route.
//!
//! Two desktops, because a process may only inject on the desktop it was created on -- measured, not
//! assumed. Both agents are started up front, so a UAC prompt appearing costs no process startup at
//! the moment responsiveness matters most.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vkms_ipc::AgentChannel;
use vkms_win::session::{self, ChildProcess, NO_SESSION};

/// Winlogon hosts the UAC prompt, the lock screen and the logon screen. Without an agent there, a
/// remote user cannot answer an elevation prompt.
pub const DESKTOPS: [&str; 2] = ["Default", "Winlogon"];

pub const AGENT_EXECUTABLE: &str = "vkms-agent.exe";

/// How often to look again without being asked. Session changes normally arrive as a notification
/// from the SCM (see [`SessionNotifier`]); this is the fallback for when one does not, and what
/// restarts an agent that died.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const RESTART_BACKOFF: Duration = Duration::from_secs(2);
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(30);

/// The channels to the agents currently running, shared with the router.
///
/// Replaced whole when an agent comes or goes, and read on every input event, so readers take a
/// read lock and never copy the list.
#[derive(Default)]
pub struct AgentChannels {
    list: RwLock<Vec<Arc<AgentChannel>>>,
}

impl AgentChannels {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `body` over the current channels.
    pub fn with<R>(&self, body: impl FnOnce(&[Arc<AgentChannel>]) -> R) -> R {
        body(&self.list.read().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    pub fn snapshot(&self) -> Vec<Arc<AgentChannel>> {
        self.with(<[Arc<AgentChannel>]>::to_vec)
    }

    pub fn desktops(&self) -> Vec<String> {
        self.with(|channels| channels.iter().map(|channel| channel.desktop().to_owned()).collect())
    }

    pub fn set(&self, channels: Vec<Arc<AgentChannel>>) {
        *self.list.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = channels;
    }
}

/// One supervised agent process, plus its restart backoff.
struct AgentProcess {
    child: Option<ChildProcess>,
    channel: Option<Arc<AgentChannel>>,
    next_attempt: Instant,
    backoff: Duration,
}

impl AgentProcess {
    fn new() -> Self {
        Self { child: None, channel: None, next_attempt: Instant::now(), backoff: Duration::ZERO }
    }

    fn is_alive(&self) -> bool {
        self.child.as_ref().is_some_and(ChildProcess::is_alive)
    }

    /// Doubles the wait before the next attempt, to a ceiling. An agent that cannot start would
    /// otherwise be retried every poll forever, filling the log.
    fn back_off(&mut self) {
        self.backoff = if self.backoff.is_zero() { RESTART_BACKOFF } else { (self.backoff * 2).min(MAX_RESTART_BACKOFF) };
        self.next_attempt = Instant::now() + self.backoff;
    }

    fn stop(&mut self) {
        if let Some(child) = self.child.take() {
            child.kill();
        }

        self.channel = None;
    }
}

/// Called with the previous and the new console session when they differ. Either may be
/// [`NO_SESSION`].
pub type SessionChangeHandler = Box<dyn Fn(u32, u32) + Send>;

enum Signal {
    Reconcile,
    Stop,
}

/// Asks the supervisor to look at the console session now rather than at its next poll.
///
/// Handed to the SCM's control handler, which hears of logons, logoffs and user switches as they
/// happen. Cheap and harmless to call for any session event: a look that finds nothing changed does
/// nothing.
#[derive(Clone)]
pub struct SessionNotifier(mpsc::Sender<Signal>);

impl SessionNotifier {
    pub fn notify(&self) {
        let _ = self.0.send(Signal::Reconcile);
    }
}

pub struct AgentSupervisor {
    signals: mpsc::Sender<Signal>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl AgentSupervisor {
    /// Starts supervising. Agents are launched from `agent_directory`, next to the service.
    ///
    /// `on_session_changed` runs on the supervisor's thread when the console passes to another
    /// session, before that session's agents are started.
    pub fn start(
        channels: Arc<AgentChannels>,
        agent_directory: PathBuf,
        on_session_changed: impl Fn(u32, u32) + Send + 'static,
    ) -> Self {
        Self::start_with(channels, agent_directory, Box::new(on_session_changed), Box::new(session::active_console_session))
    }

    fn start_with(
        channels: Arc<AgentChannels>,
        agent_directory: PathBuf,
        on_session_changed: SessionChangeHandler,
        console_session: Box<dyn Fn() -> u32 + Send>,
    ) -> Self {
        let (signals, received) = mpsc::channel();

        let thread = std::thread::Builder::new()
            .name("VirtualKmSwitch.Supervisor".into())
            .spawn(move || {
                let mut state = SupervisorState {
                    channels,
                    executable: agent_directory.join(AGENT_EXECUTABLE),
                    agents: HashMap::new(),
                    session: NO_SESSION,
                    console_session,
                    on_session_changed,
                };

                log::info!("agent supervisor started");

                loop {
                    state.reconcile();

                    match received.recv_timeout(POLL_INTERVAL) {
                        Ok(Signal::Reconcile) | Err(RecvTimeoutError::Timeout) => continue,
                        Ok(Signal::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                    }
                }

                state.stop_all();
            })
            .ok();

        Self { signals, thread: Mutex::new(thread) }
    }

    /// A handle that makes the supervisor look at the console session immediately.
    pub fn notifier(&self) -> SessionNotifier {
        SessionNotifier(self.signals.clone())
    }

    /// Stops every agent. Idempotent.
    ///
    /// Signalled rather than inferred from the channel closing, since a [`SessionNotifier`] held
    /// elsewhere keeps the channel open.
    pub fn stop(&self) {
        let _ = self.signals.send(Signal::Stop);

        if let Some(thread) = self.thread.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
            let _ = thread.join();
        }
    }
}

impl Drop for AgentSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SupervisorState {
    channels: Arc<AgentChannels>,
    executable: PathBuf,
    agents: HashMap<&'static str, AgentProcess>,
    session: u32,
    console_session: Box<dyn Fn() -> u32 + Send>,
    on_session_changed: SessionChangeHandler,
}

impl SupervisorState {
    fn reconcile(&mut self) {
        let session = (self.console_session)();

        if session != self.session {
            // A fast user switch, a logon or a logoff. Agents belong to a session, so the old ones
            // are torn down rather than left pointing at a session that no longer exists. The owner
            // hears first, while the old agents can still be told to stop blocking.
            log::info!("console session changed {} -> {}", describe(self.session), describe(session));
            (self.on_session_changed)(self.session, session);
            self.stop_all();
            self.session = session;
        }

        if session == NO_SESSION {
            return; // nobody at the console; nothing to supervise
        }

        let mut changed = false;
        for desktop in DESKTOPS {
            changed |= self.ensure_running(desktop, session);
        }

        if changed {
            self.publish();
        }
    }

    /// Returns whether the set of channels changed.
    fn ensure_running(&mut self, desktop: &'static str, session: u32) -> bool {
        let executable = self.executable.clone();
        let agent = self.agents.entry(desktop).or_insert_with(AgentProcess::new);

        if agent.is_alive() || Instant::now() < agent.next_attempt {
            return false;
        }

        let had_channel = agent.channel.is_some();
        agent.stop();

        match launch(&executable, desktop, session) {
            Ok((channel, child)) => {
                log::info!("started agent for desktop {desktop} in session {session}, pid {}", child.pid());
                agent.channel = Some(Arc::new(channel));
                agent.child = Some(child);
                agent.backoff = Duration::ZERO;
                true
            }
            Err(error) => {
                agent.back_off();
                log::error!("could not start agent for desktop {desktop}: {error}; retrying in {:?}", agent.backoff);
                had_channel
            }
        }
    }

    fn publish(&self) {
        let channels = DESKTOPS
            .iter()
            .filter_map(|desktop| self.agents.get(desktop).and_then(|agent| agent.channel.clone()))
            .collect();

        self.channels.set(channels);
    }

    fn stop_all(&mut self) {
        for agent in self.agents.values_mut() {
            agent.stop();
        }

        self.agents.clear();
        self.channels.set(Vec::new());
    }
}

fn launch(executable: &std::path::Path, desktop: &str, session: u32) -> Result<(AgentChannel, ChildProcess), String> {
    if !executable.exists() {
        return Err(format!("the session agent was not found at {}", executable.display()));
    }

    // The rings are created here, by the service, so they exist before the agent starts and it
    // never has to race to open something that is not there yet.
    let channel = AgentChannel::create(desktop, session, AgentChannel::DEFAULT_CAPACITY, true, true)
        .map_err(|error| error.to_string())?;

    let child = session::launch_as_system_in_session(
        &executable.to_string_lossy(),
        &format!("--desktop {desktop} --session {session}"),
        session,
        &format!(r"Winsta0\{desktop}"),
    )
    .map_err(|error| error.to_string())?;

    Ok((channel, child))
}

fn describe(session: u32) -> String {
    if session == NO_SESSION {
        "none".into()
    } else {
        session.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A supervisor whose console session is whatever the test says, and whose agents never start:
    /// the directory holds no agent, so each launch fails before touching anything privileged.
    fn supervise(console: &Arc<AtomicU32>) -> (AgentSupervisor, mpsc::Receiver<(u32, u32)>) {
        let (changes, heard) = mpsc::channel();
        let console = Arc::clone(console);

        let supervisor = AgentSupervisor::start_with(
            Arc::new(AgentChannels::new()),
            std::env::temp_dir().join(format!("vkms-no-agent-{}", uuid::Uuid::new_v4())),
            Box::new(move |from, to| {
                let _ = changes.send((from, to));
            }),
            Box::new(move || console.load(Ordering::SeqCst)),
        );

        (supervisor, heard)
    }

    #[test]
    fn a_notification_is_acted_on_at_once_rather_than_at_the_next_poll() {
        let console = Arc::new(AtomicU32::new(5));
        let (supervisor, heard) = supervise(&console);
        assert_eq!((NO_SESSION, 5), heard.recv_timeout(Duration::from_secs(5)).unwrap());

        console.store(6, Ordering::SeqCst);
        let notified_at = Instant::now();
        supervisor.notifier().notify();

        assert_eq!((5, 6), heard.recv_timeout(POLL_INTERVAL).unwrap());
        assert!(notified_at.elapsed() < POLL_INTERVAL / 2, "took {:?}; that was the poll, not the notification", notified_at.elapsed());

        supervisor.stop();
    }

    #[test]
    fn a_change_nobody_announced_is_still_found_by_the_poll() {
        let console = Arc::new(AtomicU32::new(5));
        let (supervisor, heard) = supervise(&console);
        assert_eq!((NO_SESSION, 5), heard.recv_timeout(Duration::from_secs(5)).unwrap());

        console.store(NO_SESSION, Ordering::SeqCst);

        assert_eq!((5, NO_SESSION), heard.recv_timeout(POLL_INTERVAL * 3).unwrap());
        supervisor.stop();
    }

    #[test]
    fn a_notification_that_changes_nothing_reports_nothing() {
        // The SCM reports locks, unlocks and remote connections too; none of those move the console.
        let console = Arc::new(AtomicU32::new(5));
        let (supervisor, heard) = supervise(&console);
        assert_eq!((NO_SESSION, 5), heard.recv_timeout(Duration::from_secs(5)).unwrap());

        for _ in 0..5 {
            supervisor.notifier().notify();
        }

        assert!(heard.recv_timeout(Duration::from_millis(300)).is_err());
        supervisor.stop();
    }

    #[test]
    fn stopping_does_not_wait_on_notifiers_still_held_elsewhere() {
        // The SCM's handler keeps a notifier for the life of the process, so the supervisor cannot
        // learn it should stop from its channel closing.
        let console = Arc::new(AtomicU32::new(5));
        let (supervisor, _heard) = supervise(&console);
        let _held = supervisor.notifier();

        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            supervisor.stop();
            let _ = done.send(());
        });

        assert!(finished.recv_timeout(Duration::from_secs(5)).is_ok(), "stop() hung");
    }
}
