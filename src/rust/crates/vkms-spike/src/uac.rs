//! The service's half of the secure-desktop check: can the real agent, placed on Winlogon the way
//! the service places it, dismiss a live UAC prompt?
//!
//! Run as SYSTEM in session 0 by `src/rust/spike/run-uac-check.ps1`, standing in for the service, which on a
//! single machine has no peer to receive input from. Everything after the ring is the production
//! path: the agent executable, its desktop gate and settle delay, and its injector. Only the source
//! of the input differs -- a loop here rather than a paired machine.
//!
//! The injected key is Escape, which *dismisses* a prompt. If the prompt closes by itself, injection
//! into the secure desktop works, and nothing was granted.

use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use log::LevelFilter;
use vkms_core::{clock, InputEvent, InputEventFlags};
use vkms_ipc::AgentChannel;
use vkms_win::session::{self, ChildProcess, NO_SESSION};

const DESKTOP: &str = "Winlogon";
const ESCAPE: u16 = 0x01;

/// Faster than a person could answer a prompt, so a prompt that closes this soon after appearing
/// was closed by the agent. The agent itself waits for the desktop to settle before injecting.
const FEED_INTERVAL: Duration = Duration::from_millis(250);

fn data_dir() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData")).join("VirtualKmSwitch")
}

/// Kills the agent however this process leaves: a SYSTEM agent outliving the check would keep
/// dismissing secure-desktop prompts with nobody watching.
struct Agent(ChildProcess);

impl Drop for Agent {
    fn drop(&mut self) {
        self.0.kill();
        log::info!("agent stopped");
    }
}

pub fn run(duration: Duration) -> i32 {
    let log_path = data_dir().join("uac-check.log");
    let _ = std::fs::remove_file(&log_path); // one run per log, so the script reads only this one
    vkms_win::logging::init_file(&log_path, LevelFilter::Info);

    log::info!("uac check starting (pid {}, {}s)", std::process::id(), duration.as_secs());

    if !vkms_win::security::is_system() {
        log::error!("FAILED: not running as SYSTEM; only SYSTEM can place an agent on the Winlogon desktop");
        return 1;
    }

    let session = session::active_console_session();
    if session == NO_SESSION {
        log::error!("FAILED: nobody is at the console");
        return 1;
    }

    // As the service does: the rings exist before the agent starts, so it never races to open them.
    let channel = match AgentChannel::create(DESKTOP, session, AgentChannel::DEFAULT_CAPACITY, true, true) {
        Ok(channel) => channel,
        Err(error) => {
            log::error!("FAILED: could not create the agent channel: {error}");
            return 1;
        }
    };

    let executable = match std::env::current_exe().map(|path| path.with_file_name("vkms-agent.exe")) {
        Ok(path) if path.exists() => path,
        _ => {
            log::error!("FAILED: vkms-agent.exe was not found beside this executable");
            return 1;
        }
    };

    let agent = match session::launch_as_system_in_session(
        &executable.to_string_lossy(),
        &format!("--desktop {DESKTOP} --session {session}"),
        session,
        &format!(r"Winsta0\{DESKTOP}"),
    ) {
        Ok(child) => Agent(child),
        Err(error) => {
            log::error!("FAILED: could not launch the agent: {error}");
            return 1;
        }
    };

    log::info!("agent started on {DESKTOP} in session {session}, pid {}", agent.0.pid());

    let deadline = Instant::now() + duration;
    let mut fed = 0u64;
    let mut drained = [InputEvent::default(); 64];

    while Instant::now() < deadline {
        if !agent.0.is_alive() {
            log::error!("FAILED: the agent exited; see agent.{DESKTOP}.log");
            return 1;
        }

        // Exactly what the router does with remote input. The agent drops it until its desktop has
        // been showing for the settle delay, and injects it from then on.
        let now = clock::ticks();
        let down = channel.to_agent.try_write(&InputEvent::key(now, ESCAPE, InputEventFlags::NONE));
        let up = channel.to_agent.try_write(&InputEvent::key(now, ESCAPE, InputEventFlags::UP));
        channel.to_agent.signal_data_available();

        if down && up {
            fed += 1;
        }

        // Nothing is expected the other way while not engaged, but a full ring would make the agent
        // stand down, so it is kept empty as the router would.
        while channel.to_service.read_batch(&mut drained) > 0 {}

        sleep(FEED_INTERVAL);
    }

    log::info!("finished: queued Escape {fed} times; the agent dropped all but those arriving while a prompt showed");
    0
}
