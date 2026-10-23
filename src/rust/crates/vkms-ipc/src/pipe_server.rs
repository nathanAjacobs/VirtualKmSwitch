//! The service's control endpoint: a named pipe the controller connects to.
//!
//! Authorisation is decided from the *caller's own token* by impersonating the connection, not from
//! anything the client sends. A client that claims to be an administrator is simply a client that
//! says so; the token is the only thing that cannot be forged.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vkms_win::security::{self, SecurityAttributes};

use crate::protocol::{self, tier_for, ControlRequest, ControlResponse, ControlTier};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Handles one control request, given the tier the caller's token earns it.
pub type ControlHandler = Arc<dyn Fn(ControlRequest, ControlTier) -> BoxFuture<ControlResponse> + Send + Sync>;

/// Concurrent listeners, so one slow client cannot lock everyone else out.
const LISTENERS: usize = 4;

pub struct ControlPipeServer {
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl ControlPipeServer {
    /// Starts listening on `\\.\pipe\<pipe_name>`. Must be called inside a Tokio runtime.
    ///
    /// The name is a parameter so that tests, and any second instance, do not silently attach to
    /// whichever server already owns the production name -- a collision that looks like a protocol
    /// bug rather than a naming one.
    pub fn start(pipe_name: &str, handler: ControlHandler) -> io::Result<Self> {
        let path = format!(r"\\.\pipe\{pipe_name}");
        let sddl = pipe_sddl()?;

        // The first instance is created here, synchronously and with FIRST_PIPE_INSTANCE, so that
        // a pipe somebody else already created under this name is an error at startup -- not a
        // quiet arrangement in which the controller may be talking to an impostor.
        let first = create_instance(&path, &sddl, true)?;

        let cancel = CancellationToken::new();
        let mut tasks = Vec::with_capacity(LISTENERS);
        let mut first = Some(first);

        for _ in 0..LISTENERS {
            let listener = Listener {
                path: path.clone(),
                sddl: sddl.clone(),
                handler: Arc::clone(&handler),
                cancel: cancel.clone(),
            };

            tasks.push(tokio::spawn(listener.run(first.take())));
        }

        Ok(Self { cancel, tasks })
    }

    /// Stops listening. Connections being served are abandoned.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();

        for task in std::mem::take(&mut self.tasks) {
            // A listener parked in connect() unblocks on cancellation; one serving a slow client is
            // not waited for indefinitely.
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
    }
}

impl Drop for ControlPipeServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// The pipe is reachable by authenticated users, because the unelevated controller must reach it at
/// all. That is deliberately *not* the authorisation boundary: connecting is allowed, and each
/// request is then checked against the caller's token.
///
/// Authenticated users get read and write but not `FILE_CREATE_PIPE_INSTANCE` (which is what
/// `GENERIC_WRITE` would also grant on a pipe), so nobody else can add an instance of this pipe and
/// sit waiting for the controller to connect to it. Network logons are refused outright.
fn pipe_sddl() -> io::Result<String> {
    // FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_EA | FILE_WRITE_EA | FILE_READ_ATTRIBUTES |
    // FILE_WRITE_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE
    const READ_WRITE: &str = "0x12019b";

    let mut sddl = format!("D:(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;{READ_WRITE};;;AU)");

    // Every listener after the first needs FILE_CREATE_PIPE_INSTANCE on the pipe the first made.
    // SYSTEM has it above; running in console mode for development the creator does not, and every
    // listener but one would fail with a bare access-denied. Adding the creator only when it is not
    // SYSTEM keeps the production DACL exactly as tight.
    if !security::is_system() {
        sddl.push_str(&format!("(A;;GA;;;{})", security::current_user_sid()?));
    }

    Ok(sddl)
}

fn create_instance(path: &str, sddl: &str, first: bool) -> io::Result<NamedPipeServer> {
    let mut attributes = SecurityAttributes::from_sddl(sddl)?;

    let mut options = ServerOptions::new();
    options.first_pipe_instance(first).reject_remote_clients(true);

    // SAFETY: the attributes outlive the call, which copies the descriptor into the pipe object.
    unsafe { options.create_with_security_attributes_raw(path, attributes.as_mut_void()) }
}

struct Listener {
    path: String,
    sddl: String,
    handler: ControlHandler,
    cancel: CancellationToken,
}

impl Listener {
    async fn run(self, mut first: Option<NamedPipeServer>) {
        while !self.cancel.is_cancelled() {
            let pipe = match first.take() {
                Some(pipe) => pipe,
                None => match create_instance(&self.path, &self.sddl, false) {
                    Ok(pipe) => pipe,
                    Err(error) => {
                        log::warn!("control pipe listener error: {error}");

                        // Back off rather than spinning if the pipe cannot be created at all.
                        tokio::select! {
                            _ = self.cancel.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                        }
                    }
                },
            };

            tokio::select! {
                _ = self.cancel.cancelled() => return,
                connected = pipe.connect() => {
                    if let Err(error) = connected {
                        log::warn!("control pipe connect failed: {error}");
                        continue;
                    }
                }
            }

            let tier = caller_tier(&pipe);

            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = serve(pipe, tier, &self.handler) => {}
            }
        }
    }
}

/// Reads the caller's privilege from the connection's own token. Failing to read it fails closed:
/// an unreadable token means the least privilege, never the most.
fn caller_tier(pipe: &NamedPipeServer) -> ControlTier {
    match security::pipe_client_is_administrator(pipe) {
        Ok(true) => ControlTier::Administrative,
        Ok(false) => ControlTier::Runtime,
        Err(error) => {
            log::warn!("could not determine caller privilege, treating as runtime: {error}");
            ControlTier::Runtime
        }
    }
}

async fn serve(mut pipe: NamedPipeServer, caller: ControlTier, handler: &ControlHandler) {
    loop {
        let payload = match protocol::read_message(&mut pipe).await {
            Ok(Some(payload)) => payload,
            Ok(None) => return,
            Err(error) => {
                log::debug!("dropping control connection: {error}");
                return;
            }
        };

        let request: ControlRequest = match serde_json::from_slice(&payload) {
            Ok(request) => request,
            Err(error) => {
                // Hostile or corrupt; there is nothing useful to say back.
                log::debug!("dropping control connection after an unreadable request: {error}");
                return;
            }
        };

        let response = if tier_for(request.verb) == ControlTier::Administrative && caller != ControlTier::Administrative {
            log::info!("refused {:?}: caller is not an administrator", request.verb);
            ControlResponse::fail(format!("{:?} requires administrator rights.", request.verb))
        } else {
            invoke(handler, request, caller).await
        };

        let Ok(bytes) = serde_json::to_vec(&response) else {
            return;
        };

        if protocol::write_message(&mut pipe, &bytes).await.is_err() {
            return;
        }
    }
}

/// Runs the handler as its own task, so a handler that panics becomes a failure response rather
/// than a dropped connection -- and certainly rather than a dead listener.
async fn invoke(handler: &ControlHandler, request: ControlRequest, caller: ControlTier) -> ControlResponse {
    match tokio::spawn(handler(request, caller)).await {
        Ok(response) => response,
        Err(error) if error.is_panic() => {
            let payload = error.into_panic();
            let message = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "the request failed".to_owned());

            ControlResponse::fail(message)
        }
        Err(_) => ControlResponse::fail("the request was cancelled"),
    }
}
