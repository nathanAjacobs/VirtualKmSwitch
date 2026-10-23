//! The Virtual KM Switch service: LocalSystem, session 0, auto-start.
//!
//! Runs either under the SCM or, with `--console`, as a foreground process for development. The two
//! share everything except where logging goes and how a stop is requested. `--stop` and `--start`
//! act on the installed service instead, for the installer.

use std::ffi::OsString;
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use log::LevelFilter;
use vkms_service::paths;
use vkms_service::service::{self, ServiceOptions};
use vkms_service::supervisor::SessionNotifier;
use windows_service::service::{
    Service, ServiceAccess, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

const SERVICE_NAME: &str = "VirtualKmSwitch";

/// How long `--stop` waits. The service asks the SCM for 15 s to stop; twice that is a hang.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;

define_windows_service!(ffi_service_main, service_main);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|arg| arg.eq_ignore_ascii_case(name));

    let value = |name: &str| {
        let index = args.iter().position(|arg| arg.eq_ignore_ascii_case(name))?;
        args.get(index + 1).cloned()
    };

    if flag("--help") || flag("-h") {
        println!("vkms-service [--console] [--no-agents] [--data-dir <path>] [--pipe <name>]");
        println!("vkms-service --stop | --start");
        println!();
        println!("  --console          run in the foreground, logging to this console, until Ctrl+C");
        println!("  --no-agents        do not launch session agents (useful unelevated, where they cannot start)");
        println!(r"  --data-dir <path>  keep state somewhere other than %ProgramData%\VirtualKmSwitch");
        println!("  --pipe <name>      serve the control pipe under another name, beside an installed service");
        println!("  --stop             stop the installed service and its agents, and wait (run by the installer)");
        println!("  --start            start the installed service again (the installer's rollback of --stop)");
        return;
    }

    if flag("--stop") || flag("--start") {
        vkms_win::logging::init_stderr(LevelFilter::Info);
        std::process::exit(if flag("--stop") { stop_installed() } else { start_installed() });
    }

    if flag("--console") {
        vkms_win::logging::init_stderr(LevelFilter::Info);

        let mut options = ServiceOptions::production();
        options.agents = !flag("--no-agents");
        if let Some(directory) = value("--data-dir") {
            options.data_dir = directory.into();
        }
        if let Some(pipe) = value("--pipe") {
            options.pipe_name = pipe;
        }

        std::process::exit(run_console(options));
    }

    vkms_win::logging::init_file(&paths::data_dir().join("service.log"), LevelFilter::Info);

    if let Err(error) = service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        // Error 1063 is "not started by the SCM": somebody ran the binary by hand.
        log::error!("could not start as a service: {error}");
        eprintln!("This is the Virtual KM Switch service. Install it with deploy\\service.ps1, or run it with --console.");
        std::process::exit(1);
    }
}

/// Stops the installed service and waits until it has, which takes its agents down with it.
///
/// The installer bundle runs this before the MSI, and it is why there is a bundle at all. Restart
/// Manager decides whether an upgrade can close what holds the files it replaces before the MSI
/// stops the service, and it cannot close the agents: SYSTEM processes in the user's session. Finding
/// them, it closes nothing at all, the controller included, and asks for a reboot instead. Nothing
/// inside an MSI can run elevated that early, so the stop has to come from outside it.
fn stop_installed() -> i32 {
    let service = match open_installed(ServiceAccess::STOP | ServiceAccess::QUERY_STATUS) {
        Ok(Some(service)) => service,
        Ok(None) => {
            log::info!("the service is not installed; nothing to stop");
            return 0;
        }
        Err(error) => {
            log::error!("could not open the service: {}", describe(&error));
            return 1;
        }
    };

    match service.stop() {
        Ok(_) => {}
        Err(error) if os_error(&error) == Some(ERROR_SERVICE_NOT_ACTIVE) => {
            log::info!("the service is already stopped");
            return 0;
        }
        Err(error) => {
            log::error!("could not stop the service: {}", describe(&error));
            return 1;
        }
    }

    // Stopped is reported only once the supervisor has ended the agents, so waiting for it is
    // waiting for them.
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        match service.query_status() {
            Ok(status) if status.current_state == ServiceState::Stopped => {
                log::info!("the service has stopped");
                return 0;
            }
            Ok(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            Ok(status) => {
                log::error!("the service is still {:?} after {STOP_TIMEOUT:?}", status.current_state);
                return 1;
            }
            Err(error) => {
                log::error!("could not query the service: {}", describe(&error));
                return 1;
            }
        }
    }
}

/// Starts the installed service, for when an install that ran `--stop` is rolled back.
fn start_installed() -> i32 {
    let service = match open_installed(ServiceAccess::START) {
        Ok(Some(service)) => service,
        Ok(None) => {
            log::info!("the service is not installed; nothing to start");
            return 0;
        }
        Err(error) => {
            log::error!("could not open the service: {}", describe(&error));
            return 1;
        }
    };

    match service.start::<&str>(&[]) {
        Ok(()) => 0,
        Err(error) if os_error(&error) == Some(ERROR_SERVICE_ALREADY_RUNNING) => 0,
        Err(error) => {
            log::error!("could not start the service: {}", describe(&error));
            1
        }
    }
}

/// The installed service, or `None` if there is none to act on.
fn open_installed(access: ServiceAccess) -> windows_service::Result<Option<Service>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => Ok(Some(service)),
        Err(error) if os_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The Windows error itself: windows-service's own message for it is only "IO error in winapi call".
fn describe(error: &windows_service::Error) -> String {
    match error {
        windows_service::Error::Winapi(io) => io.to_string(),
        other => other.to_string(),
    }
}

fn os_error(error: &windows_service::Error) -> Option<i32> {
    match error {
        windows_service::Error::Winapi(io) => io.raw_os_error(),
        _ => None,
    }
}

fn announce() {
    let system = vkms_win::security::is_system();
    log::info!("Virtual KM Switch service starting (LocalSystem={system})");

    if !system {
        // Almost everything downstream of the control pipe needs SYSTEM: creating Global objects,
        // placing agents in another session, touching the Winlogon desktop. Saying so once here
        // beats a cascade of access-denied errors that each look like a separate bug.
        log::warn!("not running as LocalSystem -- agent launch and Global shared memory will fail");
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("vkms-service").build().expect("a Tokio runtime")
}

fn run_console(options: ServiceOptions) -> i32 {
    announce();

    runtime().block_on(async {
        let running = match service::start(options).await {
            Ok(running) => running,
            Err(error) => {
                log::error!("the service could not start: {error}");
                return 1;
            }
        };

        log::info!("running; press Ctrl+C to stop");
        let _ = tokio::signal::ctrl_c().await;

        running.stop().await;
        0
    })
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service() {
        log::error!("service failed: {error}");
    }
}

fn run_service() -> windows_service::Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    // Filled once the service is up. The handler is registered before that, and a session change
    // arriving in between is caught by the supervisor's own first look.
    let sessions: Arc<OnceLock<SessionNotifier>> = Arc::default();
    let notify_sessions = Arc::clone(&sessions);

    let status_handle = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }

        // Logons, logoffs and user switches, as they happen, rather than up to a poll interval late.
        // Every reason is passed on: the supervisor compares sessions itself, so a lock or a remote
        // connection that changes nothing costs one cheap look.
        ServiceControl::SessionChange(change) => {
            log::info!("session event: {:?} in session {}", change.reason, change.notification.session_id);
            if let Some(notifier) = notify_sessions.get() {
                notifier.notify();
            }
            ServiceControlHandlerResult::NoError
        }

        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;

    let status = |state, accepted, wait_hint| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint,
        process_id: None,
    };

    status_handle.set_service_status(status(ServiceState::StartPending, ServiceControlAccept::empty(), Duration::from_secs(10)))?;
    announce();

    let runtime = runtime();
    let started = runtime.block_on(service::start(ServiceOptions::production()));

    let running = match started {
        Ok(running) => running,
        Err(error) => {
            log::error!("the service could not start: {error}");
            status_handle.set_service_status(ServiceStatus {
                exit_code: ServiceExitCode::ServiceSpecific(1),
                ..status(ServiceState::Stopped, ServiceControlAccept::empty(), Duration::ZERO)
            })?;
            return Ok(());
        }
    };

    if let Some(notifier) = running.session_notifier() {
        let _ = sessions.set(notifier);
    }

    status_handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::SESSION_CHANGE,
        Duration::ZERO,
    ))?;

    // Blocks this SCM thread until a stop arrives; the runtime's own threads do the work.
    let _ = stop_rx.recv();

    status_handle.set_service_status(status(ServiceState::StopPending, ServiceControlAccept::empty(), Duration::from_secs(15)))?;
    runtime.block_on(running.stop());
    runtime.shutdown_timeout(Duration::from_secs(5));

    status_handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty(), Duration::ZERO))?;
    log::info!("service stopped");
    Ok(())
}
