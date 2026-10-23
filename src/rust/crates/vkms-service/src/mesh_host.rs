//! Brings the mesh up, keeps it populated, runs pairing, and joins it all to the input path.
//!
//! Nothing listens until there is a reason to. A machine paired with nobody has no one to accept a
//! connection from, so it binds no mesh port; it announces itself only while pairing is under way.
//! A listener that trusts nobody would be harmless but misleading: it makes the machine look
//! reachable on a port where every connection will be refused.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use vkms_core::InputEvent;
use vkms_net::beacon::{self, BeaconMessage, DiscoveryBeacon};
use vkms_net::pairing::{self, EnrollmentOutcome, EnrollmentResult, PairingCode, PairingIdentity, PairingWindow};
use vkms_net::PeerMesh;

use crate::registry::PeerRegistry;
use crate::remote_input::RemoteInput;
use crate::router::{InputRouter, PeerSender};

/// How often to reconcile: should anything be listening, are peers still dialled.
///
/// Slow on purpose. Nothing on the latency path goes through here -- once a link exists, its pump
/// and receive loop own it -- and pairing or a change of peers kicks a pass immediately.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(2);

/// How long the machine a code is typed into listens for the machine showing it.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// A pairing advertisement older than this is ignored: the window behind it has probably closed.
const CANDIDATE_TTL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct MeshOptions {
    pub mesh_port: u16,
    pub beacon_port: u16,
    pub pairing_port: u16,

    /// How long a pairing window stays open. Bounded deliberately: a code that works indefinitely is
    /// one an attacker can use at leisure.
    pub pairing_duration: Duration,

    /// Whether to use multicast discovery at all. Off in tests, which give addresses explicitly.
    pub discovery: bool,
}

impl Default for MeshOptions {
    fn default() -> Self {
        Self {
            mesh_port: PeerMesh::DEFAULT_PORT,
            beacon_port: beacon::DEFAULT_PORT,
            pairing_port: pairing::window::DEFAULT_PORT,
            pairing_duration: Duration::from_secs(5 * 60),
            discovery: true,
        }
    }
}

struct OpenPairing {
    window: PairingWindow,
    advertised_id: String,
}

/// A machine showing a pairing code, as heard on the network.
#[derive(Clone, Copy)]
struct Candidate {
    node_id: Uuid,
    address: SocketAddr,
    heard_at: Instant,
}

pub struct MeshHost {
    registry: Arc<PeerRegistry>,
    router: Arc<InputRouter>,
    remote: Arc<RemoteInput>,
    options: MeshOptions,

    mesh: RwLock<Option<Arc<PeerMesh>>>,
    beacon: Mutex<Option<Arc<DiscoveryBeacon>>>,

    /// Where each paired machine accepts mesh connections, as last learned. Only ever an address to
    /// try: whether it becomes a link is decided by the certificate the far end presents.
    endpoints: Mutex<HashMap<Uuid, SocketAddr>>,

    /// Machines with a pairing window open, keyed by the advertised hash of their code.
    candidates: Mutex<HashMap<String, Candidate>>,

    pairing: Mutex<Option<OpenPairing>>,
    joining: AtomicUsize,

    kick: Notify,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl MeshHost {
    pub fn new(registry: Arc<PeerRegistry>, router: Arc<InputRouter>, remote: Arc<RemoteInput>, options: MeshOptions) -> Arc<Self> {
        let endpoints = registry
            .peers()
            .iter()
            .filter_map(|peer| peer.last_endpoint().map(|endpoint| (peer.node_id, endpoint)))
            .collect();

        Arc::new(Self {
            registry,
            router,
            remote,
            options,
            mesh: RwLock::new(None),
            beacon: Mutex::new(None),
            endpoints: Mutex::new(endpoints),
            candidates: Mutex::new(HashMap::new()),
            pairing: Mutex::new(None),
            joining: AtomicUsize::new(0),
            kick: Notify::new(),
            cancel: CancellationToken::new(),
            task: Mutex::new(None),
        })
    }

    /// Attaches to the router and starts reconciling. Must be called inside the runtime.
    pub fn start(self: &Arc<Self>) {
        self.router.set_sender(Some(Arc::clone(self) as Arc<dyn PeerSender>));

        // Holds the host until cancelled, which shutdown does first.
        let host = Arc::clone(self);
        let cancel = self.cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                host.reconcile().await;

                let kick = host.kick.notified();

                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = kick => {}
                    _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
                }
            }
        });

        *lock(&self.task) = Some(task);
    }

    /// Runs a reconcile pass now rather than at the next interval.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub async fn shutdown(&self) {
        self.cancel.cancel();

        let task = lock(&self.task).take();
        if let Some(task) = task {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }

        self.close_pairing_window().await;

        // Disengaged first, and while the link is still up, so the release reaches the machine being
        // driven rather than being written into a closed stream.
        self.router.disengage();
        self.router.set_sender(None);

        self.stop_mesh().await;
        self.stop_beacon().await;
    }

    fn mesh(&self) -> Option<Arc<PeerMesh>> {
        self.mesh.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    pub fn is_running(&self) -> bool {
        self.mesh().is_some()
    }

    /// The port the mesh is listening on, or 0 when it is not.
    pub fn mesh_port(&self) -> u16 {
        self.mesh().map_or(0, |mesh| mesh.port())
    }

    pub fn connected_peers(&self) -> Vec<Uuid> {
        self.mesh().map(|mesh| mesh.connected_peers()).unwrap_or_default()
    }

    pub fn discovered_peer_count(&self) -> usize {
        lock(&self.endpoints).len()
    }

    /// Records where a paired machine can be reached, and dials it if this side should.
    pub fn add_endpoint(&self, peer: Uuid, address: SocketAddr) {
        lock(&self.endpoints).insert(peer, address);
        self.kick();
    }

    async fn reconcile(self: &Arc<Self>) {
        let has_peers = !self.registry.peers().is_empty();
        let pairing = self.pairing_left().is_some() || self.joining.load(Ordering::Acquire) > 0;

        if self.options.discovery && (has_peers || pairing) {
            self.ensure_beacon().await;
        } else {
            self.stop_beacon().await;
        }

        // The mesh also runs while pairing is under way on either side: the other machine may have
        // the lower id, and will dial the port it was told the moment it has pinned us.
        if has_peers || pairing {
            self.ensure_mesh().await;
        } else {
            self.stop_mesh().await;
        }

        self.advertise();
        self.close_expired_pairing().await;

        let Some(mesh) = self.mesh() else {
            return;
        };

        // Links to machines no longer paired are dropped: revoking the pin stops new ones, and this
        // ends the one already open.
        for peer in mesh.connected_peers() {
            if !self.registry.is_paired(peer) {
                if self.router.target() == Some(peer) {
                    self.router.disengage();
                }

                log::info!("dropping the link to {peer}, which is no longer paired");
                mesh.disconnect(peer).await;
            }
        }

        let endpoints: Vec<_> = lock(&self.endpoints).iter().map(|(peer, endpoint)| (*peer, *endpoint)).collect();

        for (peer, endpoint) in endpoints {
            // Pairing is checked at dial time, not only when the address was learned: a machine
            // removed in between must not be reconnected just because it is still announcing.
            if !self.registry.is_paired(peer) || !mesh.should_dial(peer) || mesh.is_connected(peer) {
                continue;
            }

            // Concurrently: a dial to an unreachable address can take many seconds, and must not
            // hold up the others. The mesh refuses a second dial to a peer already being dialled.
            let mesh = Arc::clone(&mesh);
            let registry = Arc::clone(&self.registry);
            tokio::spawn(async move {
                if mesh.ensure_connected(peer, &endpoint.ip().to_string(), endpoint.port()).await {
                    registry.note_address(peer, endpoint);
                }
            });
        }
    }

    async fn ensure_mesh(self: &Arc<Self>) {
        if self.mesh().is_some() {
            return;
        }

        let mesh = match PeerMesh::new(
            self.registry.local_node_id(),
            self.registry.identity(),
            Arc::clone(self.registry.pins()),
            self.options.mesh_port,
        ) {
            Ok(mesh) => Arc::new(mesh),
            Err(error) => {
                log::error!("could not create the mesh: {error}");
                return;
            }
        };

        let remote = Arc::clone(&self.remote);
        mesh.on_input(move |peer, events, _| remote.receive(peer, events));

        let (remote, router) = (Arc::clone(&self.remote), Arc::clone(&self.router));
        mesh.on_connection_changed(move |peer, connected| {
            if connected {
                return;
            }

            // Whatever that machine was holding down here is let go.
            remote.source_lost(peer);

            // Auto-disengage on transport loss. A dead link while engaged leaves the user typing
            // into a machine that is no longer listening, with a keyboard that does nothing here.
            if router.target() == Some(peer) {
                log::warn!("the machine being driven went away; returning control here");
                router.disengage();
            }
        });

        let router = Arc::clone(&self.router);
        mesh.on_send_stalled(move |peer, stall| {
            log::warn!("send to {peer} stalled ({stall:?})");
            // Either way the peer cannot be trusted to receive the next key-up, so stop driving it.
            if router.target() == Some(peer) {
                router.disengage();
            }
        });

        // Built and started before being published, so a port already in use leaves nothing
        // half-built behind reporting itself as running.
        match mesh.start().await {
            Ok(port) => {
                log::info!(
                    "mesh up as {} on port {port}, trusting {} paired machine(s)",
                    self.registry.local_node_id(),
                    self.registry.pins().count()
                );
                *self.mesh.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(mesh);
            }
            Err(error) => log::error!("could not start the mesh on port {}: {error}", self.options.mesh_port),
        }
    }

    async fn stop_mesh(&self) {
        let mesh = self.mesh.write().unwrap_or_else(|poisoned| poisoned.into_inner()).take();

        if let Some(mesh) = mesh {
            if self.router.is_engaged() {
                self.router.disengage();
            }

            mesh.shutdown().await;
            log::info!("mesh stopped");
        }
    }

    async fn ensure_beacon(self: &Arc<Self>) {
        if lock(&self.beacon).is_some() {
            return;
        }

        match DiscoveryBeacon::start(self.options.beacon_port).await {
            Ok(beacon) => {
                let weak = Arc::downgrade(self);
                beacon.on_peer_seen(move |message, from| {
                    if let Some(host) = weak.upgrade() {
                        host.on_beacon(message, from);
                    }
                });

                *lock(&self.beacon) = Some(Arc::new(beacon));
            }
            Err(error) => log::warn!("could not start discovery on port {}: {error}", self.options.beacon_port),
        }
    }

    async fn stop_beacon(&self) {
        let beacon = lock(&self.beacon).take();
        if let Some(beacon) = beacon {
            beacon.shutdown().await;
        }
    }

    fn advertise(&self) {
        let Some(beacon) = lock(&self.beacon).clone() else {
            return;
        };

        let (pairing_open, pairing_port, pairing_id) = match lock(&self.pairing).as_ref() {
            Some(open) if open.window.is_open() => (true, open.window.port(), open.advertised_id.clone()),
            _ => (false, 0, String::new()),
        };

        beacon.advertise(BeaconMessage {
            node_id: self.registry.local_node_id(),
            machine_name: self.registry.display_name().to_owned(),
            port: self.mesh_port(),
            thumbprint: self.registry.identity().thumbprint().to_owned(),
            pairing_open,
            pairing_port,
            pairing_id,
        });
    }

    /// Records what a beacon says. Unauthenticated, so it only ever produces an address to try.
    fn on_beacon(&self, message: &BeaconMessage, from: SocketAddr) {
        if message.node_id == self.registry.local_node_id() {
            return;
        }

        if message.pairing_open && !message.pairing_id.is_empty() && message.pairing_port != 0 {
            lock(&self.candidates).insert(
                message.pairing_id.clone(),
                Candidate {
                    node_id: message.node_id,
                    address: SocketAddr::new(from.ip(), message.pairing_port),
                    heard_at: Instant::now(),
                },
            );
        }

        if message.port != 0 && self.registry.is_paired(message.node_id) {
            let endpoint = SocketAddr::new(from.ip(), message.port);
            let previous = lock(&self.endpoints).insert(message.node_id, endpoint);

            if previous != Some(endpoint) {
                self.kick();
            }
        }
    }

    // --- Pairing: the side showing the code ---

    /// Opens a pairing window with a fresh code. Any window already open is closed first, so only
    /// the code on screen ever works.
    pub async fn open_pairing_window(self: &Arc<Self>) -> io::Result<(String, Duration)> {
        self.close_pairing_window().await;

        // The mesh comes up first, so the port told to the other machine is a real one.
        self.ensure_mesh().await;

        let code = PairingCode::create();
        let normalized = PairingCode::normalize(&code);
        let weak = Arc::downgrade(self);

        let window = PairingWindow::open(
            PairingIdentity {
                node_id: self.registry.local_node_id(),
                display_name: self.registry.display_name().to_owned(),
                code: normalized.clone(),
                mesh_port: self.mesh_port(),
            },
            self.registry.identity(),
            self.options.pairing_duration,
            self.options.pairing_port,
            Arc::new(move |outcome| {
                if let Some(host) = Weak::upgrade(&weak) {
                    host.on_pairing_attempt(outcome);
                }
            }),
        )
        .await?;

        let remaining = window.remaining();
        *lock(&self.pairing) = Some(OpenPairing { window, advertised_id: PairingCode::advertised_id(&normalized) });
        self.kick();

        Ok((code, remaining))
    }

    pub async fn close_pairing_window(&self) {
        let open = lock(&self.pairing).take();
        if let Some(open) = open {
            open.window.shutdown().await;
            self.kick();
        }
    }

    /// The port the pairing window is listening on, while one is open.
    pub fn pairing_port(&self) -> Option<u16> {
        lock(&self.pairing).as_ref().filter(|open| open.window.is_open()).map(|open| open.window.port())
    }

    /// How long the pairing window has left, or `None` if none is open.
    pub fn pairing_left(&self) -> Option<Duration> {
        lock(&self.pairing).as_ref().filter(|open| open.window.is_open()).map(|open| open.window.remaining())
    }

    async fn close_expired_pairing(&self) {
        let expired = {
            let mut pairing = lock(&self.pairing);
            if pairing.as_ref().is_some_and(|open| !open.window.is_open()) {
                pairing.take()
            } else {
                None
            }
        };

        if let Some(open) = expired {
            open.window.shutdown().await;
        }
    }

    fn on_pairing_attempt(&self, outcome: &EnrollmentOutcome) {
        if !outcome.accepted() {
            return;
        }

        let endpoint = outcome
            .peer_address
            .filter(|_| outcome.peer_mesh_port != 0)
            .map(|address| SocketAddr::new(address.ip(), outcome.peer_mesh_port));

        if let Err(error) =
            self.registry.add_peer(outcome.peer_node_id, &outcome.peer_thumbprint, &outcome.peer_display_name, endpoint)
        {
            log::error!("could not record the pairing with {}: {error}", outcome.peer_node_id);
            return;
        }

        if let Some(endpoint) = endpoint {
            lock(&self.endpoints).insert(outcome.peer_node_id, endpoint);
        }

        // One machine per window: the code has done its job, and leaving it open only leaves it
        // exposed.
        if let Some(open) = lock(&self.pairing).as_ref() {
            open.window.close();
        }

        self.kick();
    }

    // --- Pairing: the side the code is typed into ---

    /// Pairs with the machine showing `code`, found by its beacon or at `host` (`name` or
    /// `name:port`) where multicast does not reach. Returns the machine paired with.
    pub async fn pair_with_code(self: &Arc<Self>, code: &str, host: Option<&str>) -> Result<EnrollmentOutcome, String> {
        let normalized = PairingCode::normalize(code);
        if normalized.chars().count() != PairingCode::SIGNIFICANT_CHARACTERS {
            return Err(format!(
                "A pairing code has {} characters, in four groups of four, like ABCD-EFGH-JKLM-NPQR.",
                PairingCode::SIGNIFICANT_CHARACTERS
            ));
        }

        // Counted for the whole attempt, so the mesh -- whose port the other machine is about to be
        // told -- is not stopped underneath it for having no peers yet.
        let _joining = Joining::new(self);

        let target = match host.map(str::trim).filter(|host| !host.is_empty()) {
            Some(host) => resolve(host, self.options.pairing_port)?,
            None => self.discover(&PairingCode::advertised_id(&normalized)).await?,
        };

        // The mesh first, so the port told to the other machine is a real one.
        self.ensure_mesh().await;

        let outcome = pairing::join(
            &target.ip().to_string(),
            target.port(),
            &normalized,
            self.registry.local_node_id(),
            self.registry.identity(),
            self.registry.display_name(),
            self.mesh_port(),
        )
        .await
        .map_err(|error| format!("Could not reach the other machine at {target}: {error}"))?;

        if !outcome.accepted() {
            return Err(describe_refusal(&outcome));
        }

        let endpoint = (outcome.peer_mesh_port != 0).then(|| SocketAddr::new(target.ip(), outcome.peer_mesh_port));

        self.registry
            .add_peer(outcome.peer_node_id, &outcome.peer_thumbprint, &outcome.peer_display_name, endpoint)
            .map_err(|error| format!("Paired, but the result could not be saved: {error}"))?;

        if let Some(endpoint) = endpoint {
            lock(&self.endpoints).insert(outcome.peer_node_id, endpoint);
        }

        self.kick();
        Ok(outcome)
    }

    /// Listens for the machine advertising `advertised_id`.
    async fn discover(self: &Arc<Self>, advertised_id: &str) -> Result<SocketAddr, String> {
        if !self.options.discovery {
            return Err("Discovery is turned off; give the other machine's address.".into());
        }

        self.ensure_beacon().await;
        self.advertise();

        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        let found = loop {
            let candidate = lock(&self.candidates).get(advertised_id).copied();

            if let Some(candidate) = candidate.filter(|candidate| candidate.heard_at.elapsed() < CANDIDATE_TTL) {
                if candidate.node_id != self.registry.local_node_id() {
                    break Some(candidate.address);
                }
            }

            if Instant::now() >= deadline {
                break None;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        found.ok_or_else(|| {
            "No machine on this network is showing that code. Check that pairing is still open on the other \
             machine, or enter its address if this network blocks discovery."
                .into()
        })
    }
}

/// Marks a pairing attempt in progress for as long as it lives.
struct Joining<'a>(&'a MeshHost);

impl<'a> Joining<'a> {
    fn new(host: &'a MeshHost) -> Self {
        host.joining.fetch_add(1, Ordering::AcqRel);
        Self(host)
    }
}

impl Drop for Joining<'_> {
    fn drop(&mut self) {
        self.0.joining.fetch_sub(1, Ordering::AcqRel);
        self.0.kick();
    }
}

impl PeerSender for MeshHost {
    fn try_send(&self, peer: Uuid, event: &InputEvent) -> bool {
        self.mesh.read().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().is_some_and(|mesh| mesh.try_send(peer, event))
    }

    fn is_connected(&self, peer: Uuid) -> bool {
        self.mesh.read().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().is_some_and(|mesh| mesh.is_connected(peer))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn resolve(host: &str, default_port: u16) -> Result<SocketAddr, String> {
    if let Ok(address) = host.parse::<SocketAddr>() {
        return Ok(address);
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, default_port));
    }

    let with_port = if host.contains(':') { host.to_owned() } else { format!("{host}:{default_port}") };
    with_port
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.find(SocketAddr::is_ipv4))
        .ok_or_else(|| format!("'{host}' is not an address this machine can resolve."))
}

/// The sentence the user reads when pairing is refused. Each cause gets its own, because each wants
/// a different reaction.
fn describe_refusal(outcome: &EnrollmentOutcome) -> String {
    match outcome.result {
        EnrollmentResult::WrongKey => {
            "The code was not accepted. Check it against the code shown on the other machine.".into()
        }
        EnrollmentResult::CertificateMismatch => outcome.detail.clone().unwrap_or_else(|| {
            "The connection to the other machine is not direct, so pairing was refused.".into()
        }),
        EnrollmentResult::PairingWindowClosed => {
            format!("The other machine is not accepting pairings. {}", outcome.detail.clone().unwrap_or_default())
        }
        _ => format!("Pairing failed: {}", outcome.detail.clone().unwrap_or_else(|| "no reason given".into())),
    }
}
