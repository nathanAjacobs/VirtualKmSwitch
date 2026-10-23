//! The interval during which this machine will talk to a machine it does not yet trust.
//!
//! Pairing gets a listener of its own that exists only while the window is open. A flag on the mesh
//! listener would have been less code and considerably worse: the mesh port would then carry a code
//! path reachable by anyone, guarded by a boolean every future change would have to keep correct. A
//! socket that is not bound cannot be reached by a mistake.
//!
//! The window is short, deliberate and visible. Without it the code would be exposed to guessing for
//! as long as the machine is switched on; with it, the exposure is an interval a person opened on
//! purpose and can watch close again.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rustls::ServerConfig;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::code::PairingCode;
use super::handshake::{self, EnrollmentResult, PROOF_SIZE};
use super::protocol::{self, EnrollAck, EnrollChallenge, EnrollConfirmation, EnrollRequest, STEP_TIMEOUT};
use crate::identity::NodeIdentity;
use crate::tls;

pub const DEFAULT_PORT: u16 = 42101;

/// Attempts allowed before the window closes itself.
///
/// Closing rather than throttling. Throttling assumes the caller will eventually succeed and is worth
/// waiting for; here, a run of failures against an eighty-bit code is not somebody mistyping, and
/// the safe answer to being probed is to stop being reachable.
const MAX_ATTEMPTS: usize = 10;

/// What the machine showing the code says about itself.
#[derive(Clone, Debug)]
pub struct PairingIdentity {
    pub node_id: Uuid,
    pub display_name: String,

    /// Normalised, as [`PairingCode::normalize`] leaves it.
    pub code: String,

    /// Where this machine accepts mesh connections, told to the machine that pairs.
    pub mesh_port: u16,
}

/// What a completed or abandoned pairing attempt produced.
#[derive(Clone, Debug)]
pub struct EnrollmentOutcome {
    pub result: EnrollmentResult,
    pub peer_node_id: Uuid,
    pub peer_thumbprint: String,
    pub peer_display_name: String,

    /// Where the other machine accepts mesh connections.
    pub peer_mesh_port: u16,

    /// Where the other machine connected from, or was reached at.
    pub peer_address: Option<SocketAddr>,

    /// Human-readable detail for the failures that have more than one cause.
    pub detail: Option<String>,
}

impl EnrollmentOutcome {
    pub fn accepted(&self) -> bool {
        self.result == EnrollmentResult::Accepted
    }

    fn refused(result: EnrollmentResult, detail: impl Into<Option<String>>) -> Self {
        Self {
            result,
            peer_node_id: Uuid::nil(),
            peer_thumbprint: String::new(),
            peer_display_name: String::new(),
            peer_mesh_port: 0,
            peer_address: None,
            detail: detail.into(),
        }
    }
}

pub type AttemptHandler = Arc<dyn Fn(&EnrollmentOutcome) + Send + Sync>;

pub struct PairingWindow {
    port: u16,
    closes_at: Instant,
    attempts: Arc<AtomicUsize>,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl PairingWindow {
    /// Opens the window: binds `port` (0 for ephemeral) and accepts attempts until `duration`
    /// elapses, the attempt limit is reached, or [`Self::close`] is called.
    ///
    /// `on_attempt` sees every attempt, accepted or not, so the owner can record a success and the
    /// interface can show what happened.
    pub async fn open(
        identity: PairingIdentity,
        tls_identity: &NodeIdentity,
        duration: Duration,
        port: u16,
        on_attempt: AttemptHandler,
    ) -> io::Result<Self> {
        if identity.code.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a pairing window needs a code"));
        }

        let config = tls::accept_any_server_config(tls_identity).map_err(io::Error::other)?;
        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        let port = listener.local_addr()?.port();

        let attempts = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let closes_at = Instant::now() + duration;

        let accepter = Accepter {
            identity,
            local_thumbprint: tls_identity.thumbprint().to_owned(),
            config,
            attempts: Arc::clone(&attempts),
            cancel: cancel.clone(),
            on_attempt,
        };

        let task = tokio::spawn(accepter.run(listener, closes_at));
        log::info!("pairing window open on port {port} for {}s", duration.as_secs());

        Ok(Self { port, closes_at, attempts, cancel, task: Mutex::new(Some(task)) })
    }

    /// The port actually bound.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn closes_at(&self) -> Instant {
        self.closes_at
    }

    pub fn remaining(&self) -> Duration {
        self.closes_at.saturating_duration_since(Instant::now())
    }

    pub fn is_open(&self) -> bool {
        !self.cancel.is_cancelled() && Instant::now() < self.closes_at
    }

    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }

    /// Closes the window early, as the owner does as soon as one machine has paired.
    pub fn close(&self) {
        self.cancel.cancel();
    }

    /// Closes the window and waits for the listener to be released.
    pub async fn shutdown(&self) {
        self.cancel.cancel();

        let task = self.task.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        if let Some(task) = task {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }

        log::info!("pairing window closed");
    }
}

impl Drop for PairingWindow {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Accepter {
    identity: PairingIdentity,
    local_thumbprint: String,
    config: Arc<ServerConfig>,
    attempts: Arc<AtomicUsize>,
    cancel: CancellationToken,
    on_attempt: AttemptHandler,
}

impl Accepter {
    /// Owns the listener, so the port is released the moment this returns.
    async fn run(self, listener: TcpListener, closes_at: Instant) {
        let deadline = tokio::time::Instant::from_std(closes_at);

        loop {
            let accepted = tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep_until(deadline) => {
                    self.cancel.cancel();
                    return;
                }
                accepted = listener.accept() => accepted,
            };

            let Ok((tcp, from)) = accepted else {
                continue;
            };

            if self.attempts.fetch_add(1, Ordering::Relaxed) + 1 > MAX_ATTEMPTS {
                log::warn!("pairing window closing: too many attempts");
                self.cancel.cancel();
                return;
            }

            // One at a time, deliberately: a pairing is attended, and serialising attempts is what
            // makes the attempt limit mean something.
            let outcome = tokio::select! {
                _ = self.cancel.cancelled() => return,
                outcome = self.accept_one(tcp, from) => outcome,
            };

            log::info!("pairing attempt from {from}: {:?}", outcome.result);
            (self.on_attempt)(&outcome);
        }
    }

    async fn accept_one(&self, tcp: TcpStream, from: SocketAddr) -> EnrollmentOutcome {
        let mut outcome = match tokio::time::timeout(STEP_TIMEOUT, TlsAcceptor::from(Arc::clone(&self.config)).accept(tcp)).await {
            Ok(Ok(stream)) => {
                let mut stream = tokio_rustls::TlsStream::from(stream);
                let outcome = self.exchange(&mut stream).await;
                let _ = stream.shutdown().await;
                outcome
            }
            Ok(Err(error)) => EnrollmentOutcome::refused(EnrollmentResult::Malformed, format!("TLS failed: {error}")),
            Err(_) => EnrollmentOutcome::refused(EnrollmentResult::Malformed, "TLS timed out".to_owned()),
        };

        outcome.peer_address = Some(from);
        outcome
    }

    async fn exchange(&self, stream: &mut tokio_rustls::TlsStream<TcpStream>) -> EnrollmentOutcome {
        let Some(joiner_thumbprint) = tls::peer_thumbprint(stream.get_ref().1) else {
            return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "the joiner presented no certificate".to_owned());
        };

        let Some(request) = protocol::read::<_, EnrollRequest>(stream).await else {
            return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "unreadable request".to_owned());
        };

        let Some(initiator_nonce) = decode(&request.initiator_nonce).filter(|_| !request.node_id.is_nil()) else {
            return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "unreadable request".to_owned());
        };

        let responder_nonce = handshake::create_nonce();

        // Ordered as this side sees it: our certificate local, theirs remote. The verifying side has
        // to reverse that, and getting it backwards produces a proof that fails for every honest
        // peer and looks exactly like a wrong code.
        let proof = handshake::compute_proof(
            &self.identity.code,
            &self.local_thumbprint,
            &joiner_thumbprint,
            &initiator_nonce,
            &responder_nonce,
        );

        let challenge = EnrollChallenge {
            node_id: self.identity.node_id,
            display_name: self.identity.display_name.clone(),
            mesh_port: self.identity.mesh_port,
            responder_nonce: BASE64.encode(responder_nonce),
            proof: BASE64.encode(proof),
            observed_peer_thumbprint: joiner_thumbprint.clone(),
            refusal: None,
        };

        if protocol::write(stream, &challenge).await.is_err() {
            return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "the joiner went away".to_owned());
        }

        let Some(confirmation) = protocol::read::<_, EnrollConfirmation>(stream).await else {
            // The joiner rejected our proof, or gave up. Either way it has its own reason to report.
            return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "no confirmation from the joiner".to_owned());
        };

        let Some(received) = decode(&confirmation.proof).filter(|proof| proof.len() == PROOF_SIZE) else {
            return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "unreadable confirmation".to_owned());
        };

        // The joiner computed its proof with its own certificate as the local one, so the two
        // thumbprints go in swapped relative to the proof sent above.
        let result = handshake::verify(
            &self.identity.code,
            &joiner_thumbprint,
            &self.local_thumbprint,
            &initiator_nonce,
            &responder_nonce,
            &received,
        );

        let outcome = if result == EnrollmentResult::Accepted {
            EnrollmentOutcome {
                result,
                peer_node_id: request.node_id,
                peer_thumbprint: joiner_thumbprint,
                peer_display_name: request.display_name,
                peer_mesh_port: request.mesh_port,
                peer_address: None,
                detail: None,
            }
        } else {
            diagnose(result, &confirmation.observed_peer_thumbprint, &self.local_thumbprint)
        };

        let ack = EnrollAck { accepted: outcome.accepted(), detail: outcome.detail.clone() };
        let _ = protocol::write(stream, &ack).await;

        outcome
    }
}

/// The joining half: connects to a machine with its window open and proves knowledge of the code.
///
/// Symmetric with the accepting half, and deliberately in the same file. The two proofs use the same
/// nonces with the thumbprints in opposite orders, which is what stops one side's proof being
/// replayed back at it -- and it is the single easiest thing in this protocol to get subtly wrong,
/// so both orderings are written where they can be read together.
///
/// A network failure is an `Err`; anything the protocol decided is an `Ok` outcome.
pub async fn join(
    host: &str,
    port: u16,
    code: &str,
    local_node_id: Uuid,
    tls_identity: &NodeIdentity,
    display_name: &str,
    mesh_port: u16,
) -> io::Result<EnrollmentOutcome> {
    let code = PairingCode::normalize(code);
    let local_thumbprint = tls_identity.thumbprint().to_owned();
    let config = tls::accept_any_client_config(tls_identity).map_err(io::Error::other)?;

    let tcp = tokio::time::timeout(STEP_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out connecting"))??;
    let address = tcp.peer_addr()?;

    let stream = tokio::time::timeout(STEP_TIMEOUT, TlsConnector::from(config).connect(tls::server_name(), tcp))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out negotiating TLS"))??;
    let mut stream = tokio_rustls::TlsStream::from(stream);

    let mut outcome = join_exchange(&mut stream, &code, local_node_id, &local_thumbprint, display_name, mesh_port).await;
    let _ = stream.shutdown().await;

    outcome.peer_address = Some(address);
    Ok(outcome)
}

async fn join_exchange(
    stream: &mut tokio_rustls::TlsStream<TcpStream>,
    code: &str,
    local_node_id: Uuid,
    local_thumbprint: &str,
    display_name: &str,
    mesh_port: u16,
) -> EnrollmentOutcome {
    let Some(accepter_thumbprint) = tls::peer_thumbprint(stream.get_ref().1) else {
        return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "the other machine presented no certificate".to_owned());
    };

    let initiator_nonce = handshake::create_nonce();
    let request = EnrollRequest {
        node_id: local_node_id,
        display_name: display_name.to_owned(),
        mesh_port,
        initiator_nonce: BASE64.encode(initiator_nonce),
    };

    if protocol::write(stream, &request).await.is_err() {
        return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "the other machine went away".to_owned());
    }

    let Some(challenge) = protocol::read::<_, EnrollChallenge>(stream).await else {
        return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "no reply from the other machine".to_owned());
    };

    if let Some(refusal) = challenge.refusal {
        return EnrollmentOutcome::refused(EnrollmentResult::PairingWindowClosed, refusal);
    }

    let (Some(responder_nonce), Some(their_proof)) = (decode(&challenge.responder_nonce), decode(&challenge.proof)) else {
        return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "unreadable challenge".to_owned());
    };

    // Verified against the certificate this side actually observed, never against anything the
    // other machine said about itself. An interceptor can relay a genuine proof; it cannot make
    // that proof match the certificate it had to present in order to sit in the middle.
    let theirs = handshake::verify(code, &accepter_thumbprint, local_thumbprint, &initiator_nonce, &responder_nonce, &their_proof);

    if theirs != EnrollmentResult::Accepted {
        return diagnose(theirs, &challenge.observed_peer_thumbprint, local_thumbprint);
    }

    let our_proof = handshake::compute_proof(code, local_thumbprint, &accepter_thumbprint, &initiator_nonce, &responder_nonce);
    let confirmation = EnrollConfirmation {
        proof: BASE64.encode(our_proof),
        observed_peer_thumbprint: accepter_thumbprint.clone(),
    };

    if protocol::write(stream, &confirmation).await.is_err() {
        return EnrollmentOutcome::refused(EnrollmentResult::Malformed, "the other machine went away".to_owned());
    }

    match protocol::read::<_, EnrollAck>(stream).await {
        Some(ack) if ack.accepted => EnrollmentOutcome {
            result: EnrollmentResult::Accepted,
            peer_node_id: challenge.node_id,
            peer_thumbprint: accepter_thumbprint,
            peer_display_name: challenge.display_name,
            peer_mesh_port: challenge.mesh_port,
            peer_address: None,
            detail: None,
        },
        Some(ack) => EnrollmentOutcome::refused(
            EnrollmentResult::WrongKey,
            ack.detail.unwrap_or_else(|| "the other machine refused the pairing".to_owned()),
        ),
        None => EnrollmentOutcome::refused(EnrollmentResult::Malformed, "the other machine did not confirm".to_owned()),
    }
}

/// Turns a failed proof into the most specific thing that can honestly be said about it.
///
/// A bad MAC has two very different causes, and telling them apart matters: one means the code was
/// mistyped, the other that somebody is sitting between these two machines. Reporting the second as
/// a wrong code would send the user off to retype it while an interceptor waits for them to try
/// again. The thumbprint the peer reports seeing is not trusted for anything -- the attempt has been
/// refused either way.
fn diagnose(result: EnrollmentResult, reported_thumbprint: &str, actual_thumbprint: &str) -> EnrollmentOutcome {
    if result == EnrollmentResult::WrongKey
        && !reported_thumbprint.is_empty()
        && !reported_thumbprint.eq_ignore_ascii_case(actual_thumbprint)
    {
        return EnrollmentOutcome::refused(
            EnrollmentResult::CertificateMismatch,
            "The other machine saw a different certificate than the one this machine presented, so the connection \
             is not direct. Do not retype the code."
                .to_owned(),
        );
    }

    EnrollmentOutcome::refused(result, None)
}

fn decode(encoded: &str) -> Option<Vec<u8>> {
    BASE64.decode(encoded.trim()).ok().filter(|bytes| !bytes.is_empty())
}
