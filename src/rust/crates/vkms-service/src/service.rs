//! The service's composition: what is created, in what order, and how the pieces are joined.
//!
//! Shared by the real service and by `--console`, so what is tested interactively is the code the
//! SCM runs.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use vkms_core::hotkeys::HotkeyActionKind;
use vkms_ipc::pipe_server::ControlPipeServer;
use vkms_net::NodeIdentity;

use crate::control::{try_engage, ControlEndpoint};
use crate::mesh_host::{MeshHost, MeshOptions};
use crate::paths;
use crate::registry::PeerRegistry;
use crate::remote_input::RemoteInput;
use crate::router::{InputRouter, RouterThread};
use crate::supervisor::{AgentChannels, AgentSupervisor, SessionNotifier};

#[derive(Clone, Debug)]
pub struct ServiceOptions {
    pub data_dir: PathBuf,
    pub pipe_name: String,

    /// Whether to launch session agents. Off for tests, and pointless without LocalSystem.
    pub agents: bool,
    pub agent_directory: PathBuf,

    /// What this machine calls itself to machines it pairs with.
    pub display_name: String,

    pub mesh: MeshOptions,
}

impl ServiceOptions {
    pub fn production() -> Self {
        Self {
            data_dir: paths::data_dir(),
            pipe_name: vkms_ipc::protocol::PIPE_NAME.to_owned(),
            agents: true,
            agent_directory: std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(PathBuf::from))
                .unwrap_or_default(),
            display_name: machine_name(),
            mesh: MeshOptions::default(),
        }
    }
}

pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this machine".into())
}

pub struct RunningService {
    pub registry: Arc<PeerRegistry>,
    pub router: Arc<InputRouter>,
    pub mesh: Arc<MeshHost>,
    pub channels: Arc<AgentChannels>,
    pub endpoint: Arc<ControlEndpoint>,
    control: Option<ControlPipeServer>,
    supervisor: Option<AgentSupervisor>,
    router_thread: Option<RouterThread>,
}

/// Starts everything. Must be called inside a Tokio runtime.
pub async fn start(options: ServiceOptions) -> io::Result<RunningService> {
    paths::ensure_data_dir(&options.data_dir)?;

    let data_dir = options.data_dir.clone();
    let registry = Arc::new(PeerRegistry::open(&options.data_dir, &options.display_name, |node_id| {
        NodeIdentity::load_or_create(&data_dir, node_id)
    })?);

    // Shared by the supervisor, which fills it, and the router, which reads it on every event.
    let channels = Arc::new(AgentChannels::new());
    let router = Arc::new(InputRouter::new(Arc::clone(&channels)));
    let remote = Arc::new(RemoteInput::new(Arc::clone(&router)));
    let mesh = MeshHost::new(Arc::clone(&registry), Arc::clone(&router), remote, options.mesh.clone());

    wire_hotkeys(&registry, &router, &mesh);

    // A target that cannot take a key event is released rather than fed a stream with holes in it.
    let weak_router = Arc::downgrade(&router);
    router.on_target_stalled(move |peer| {
        if let Some(router) = weak_router.upgrade() {
            log::warn!("{peer} could not take an event that may not be dropped; disengaging");
            router.disengage();
        }
    });

    mesh.start();
    let router_thread = router.start();

    let endpoint = Arc::new(ControlEndpoint {
        registry: Arc::clone(&registry),
        router: Arc::clone(&router),
        mesh: Arc::clone(&mesh),
        channels: Arc::clone(&channels),
    });

    let control = ControlPipeServer::start(&options.pipe_name, endpoint.handler())?;
    log::info!("control pipe listening on {}", options.pipe_name);

    let supervisor = options.agents.then(|| {
        AgentSupervisor::start(Arc::clone(&channels), options.agent_directory.clone(), release_on_session_change(&router))
    });

    Ok(RunningService {
        registry,
        router,
        mesh,
        channels,
        endpoint,
        control: Some(control),
        supervisor,
        router_thread: Some(router_thread),
    })
}

/// Stops driving another machine when the console passes to another session.
///
/// Whoever sits down next did not choose that target, and would otherwise find their keyboard
/// blocked and forwarded to it -- even at the logon screen. Disengaging also sends the target a
/// release for everything held, which the outgoing agents cannot: they are killed, not asked to
/// stop.
fn release_on_session_change(router: &Arc<InputRouter>) -> impl Fn(u32, u32) + Send + 'static {
    let router = Arc::downgrade(router);

    move |_, _| {
        if let Some(router) = router.upgrade() {
            if router.is_engaged() {
                log::info!("the console changed hands; returning control to this machine");
            }
            router.disengage();
        }
    }
}

/// Turns hotkeys matched by an agent into engagement. Runs on the router's drain thread.
fn wire_hotkeys(registry: &Arc<PeerRegistry>, router: &Arc<InputRouter>, mesh: &Arc<MeshHost>) {
    let registry: Weak<PeerRegistry> = Arc::downgrade(registry);
    let weak_router: Weak<InputRouter> = Arc::downgrade(router);
    let mesh: Weak<MeshHost> = Arc::downgrade(mesh);

    router.on_hotkey(move |action, slot| {
        let (Some(registry), Some(router), Some(mesh)) = (registry.upgrade(), weak_router.upgrade(), mesh.upgrade()) else {
            return;
        };

        match action {
            HotkeyActionKind::Engage => {
                // The slot means whatever *this* machine's own map says it means.
                let Some(peer) = registry.settings().load().target_for(slot) else {
                    log::info!("Ctrl+Alt+{slot} pressed, but no machine is assigned to it");
                    return;
                };

                if let Err(reason) = try_engage(&registry, &mesh, &router, peer) {
                    log::info!("Ctrl+Alt+{slot} pressed: {reason}");
                }
            }

            HotkeyActionKind::ReturnLocal => router.disengage(),

            HotkeyActionKind::Panic => {
                log::warn!("PANIC: the agent has released everything; returning control here");
                router.disengage();
            }

            HotkeyActionKind::None => {}
        }
    });
}

impl RunningService {
    /// Wakes the agent supervisor to look at the console session now. `None` when agents are off.
    pub fn session_notifier(&self) -> Option<SessionNotifier> {
        self.supervisor.as_ref().map(AgentSupervisor::notifier)
    }

    /// Stops everything, releasing what this machine holds on others first.
    pub async fn stop(mut self) {
        log::info!("service stopping");

        if let Some(control) = self.control.take() {
            control.shutdown().await;
        }

        self.mesh.shutdown().await;

        // Agents last: once they are gone this machine's own keyboard is no longer blocked, and that
        // should be the final state whatever happened above.
        drop(self.router_thread.take());
        if let Some(supervisor) = self.supervisor.take() {
            tokio::task::spawn_blocking(move || supervisor.stop()).await.ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use uuid::Uuid;
    use vkms_core::{InputEvent, InputEventKind};

    use super::*;
    use crate::router::PeerSender;

    #[derive(Default)]
    struct RecordingSender {
        sent: Mutex<Vec<(Uuid, InputEvent)>>,
    }

    impl PeerSender for RecordingSender {
        fn try_send(&self, peer: Uuid, event: &InputEvent) -> bool {
            self.sent.lock().unwrap().push((peer, *event));
            true
        }

        fn is_connected(&self, _peer: Uuid) -> bool {
            true
        }
    }

    #[test]
    fn a_change_of_console_session_stops_driving_and_releases_the_target() {
        let router = Arc::new(InputRouter::new(Arc::new(AgentChannels::new())));
        let sender = Arc::new(RecordingSender::default());
        router.set_sender(Some(sender.clone()));

        let peer = Uuid::new_v4();
        router.engage(peer);

        release_on_session_change(&router)(1, 2);

        assert!(!router.is_engaged(), "the next person at the console would be typing into {peer}");
        let (to, last) = *sender.sent.lock().unwrap().last().expect("a release was sent");
        assert_eq!((peer, InputEventKind::RELEASE_ALL), (to, last.kind));
    }

    #[test]
    fn a_change_of_console_session_while_not_driving_is_harmless() {
        let router = Arc::new(InputRouter::new(Arc::new(AgentChannels::new())));
        let sender = Arc::new(RecordingSender::default());
        router.set_sender(Some(sender.clone()));

        release_on_session_change(&router)(1, 2);

        assert!(!router.is_engaged());
        assert!(sender.sent.lock().unwrap().is_empty());
    }
}
