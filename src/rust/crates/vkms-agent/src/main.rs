//! Entry point for a session agent.
//!
//! Launched by the service, never directly: it has to be created as SYSTEM, inside the interactive
//! session, and on a specific desktop, none of which a process can arrange for itself. It has no
//! console, so everything goes to a log file.

#![windows_subsystem = "windows"]

use std::path::PathBuf;

use log::LevelFilter;
use vkms_agent::host::AgentHost;
use vkms_ipc::AgentChannel;

fn data_dir() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData")).join("VirtualKmSwitch")
}

fn value_of(args: &[String], name: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg.eq_ignore_ascii_case(name))?;
    args.get(index + 1).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let desktop = value_of(&args, "--desktop").unwrap_or_else(|| "Default".into());

    vkms_win::logging::init_file(&data_dir().join(format!("agent.{desktop}.log")), LevelFilter::Info);

    let Some(session) = value_of(&args, "--session").and_then(|text| text.parse::<u32>().ok()) else {
        log::error!("a valid --session is required");
        std::process::exit(2);
    };

    log::info!("=== agent starting === desktop={desktop} session={session} pid={}", std::process::id());

    if !vkms_win::security::is_system() {
        // Not fatal for development, but on the Winlogon desktop nothing will work: only SYSTEM may
        // touch it. Saying so turns a baffling silence into a clear cause.
        log::warn!("not running as SYSTEM; the secure desktop will be inaccessible");
    }

    let channel = match AgentChannel::open(&desktop, session, AgentChannel::DEFAULT_CAPACITY, true) {
        Ok(channel) => channel,
        Err(error) => {
            log::error!("AGENT FAILED: could not open the channel to the service: {error}");
            std::process::exit(1);
        }
    };

    let _host = match AgentHost::start(&desktop, channel) {
        Ok(host) => host,
        Err(error) => {
            log::error!("AGENT FAILED: {error}");
            std::process::exit(1);
        }
    };

    log::info!("agent running");

    // The service owns this process's lifetime and terminates it on session change or shutdown.
    loop {
        std::thread::park();
    }
}
