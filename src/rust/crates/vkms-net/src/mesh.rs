//! Links to every paired machine that can be reached.
//!
//! A full mesh rather than a star through any one machine, because targeting is per machine: input
//! goes straight from the machine you are typing on to the machine you are driving, with no relay
//! hop on the latency path.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use rustls::{ClientConfig, ServerConfig};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use vkms_core::InputEvent;

use crate::connection::PeerConnection;
use crate::identity::NodeIdentity;
use crate::pins::PinStore;
use crate::send_pump::{self, PeerSendPump, SendPumpStall};
use crate::tls;

pub type InputHandler = Arc<dyn Fn(Uuid, &[InputEvent], i64) + Send + Sync>;
pub type ControlHandler = Arc<dyn Fn(Uuid, &[u8]) + Send + Sync>;
pub type ConnectionHandler = Arc<dyn Fn(Uuid, bool) + Send + Sync>;
pub type StallHandler = Arc<dyn Fn(Uuid, SendPumpStall) + Send + Sync>;

/// Heartbeat spacing while a link's clock estimate is still settling.
const WARMUP_INTERVAL: Duration = Duration::from_millis(100);

/// Heartbeat spacing once it has. Tracks drift, nothing faster.
const STEADY_INTERVAL: Duration = Duration::from_secs(5);
const WARMUP_TICKS: u32 = 12;

/// Round trips taken as soon as a link is usable, before relying on its clock.
const CLOCK_SEED_PINGS: usize = 3;

/// An inbound connection that has not finished its handshake in this long is dropped.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

struct Link {
    connection: Arc<PeerConnection>,
    pump: Arc<PeerSendPump<PeerConnection>>,
}

#[derive(Default)]
struct Handlers {
    input: RwLock<Vec<InputHandler>>,
    control: RwLock<Vec<ControlHandler>>,
    connection: RwLock<Vec<ConnectionHandler>>,
    stall: RwLock<Vec<StallHandler>>,
}

fn each<T>(handlers: &RwLock<Vec<T>>, mut call: impl FnMut(&T)) {
    for handler in handlers.read().unwrap_or_else(|poisoned| poisoned.into_inner()).iter() {
        call(handler);
    }
}

struct Inner {
    node_id: Uuid,
    pins: Arc<PinStore>,
    requested_port: u16,
    bound_port: AtomicU16,
    client_config: Arc<ClientConfig>,
    server_config: Arc<ServerConfig>,

    /// Read on every captured event, written when a link comes or goes.
    links: RwLock<HashMap<Uuid, Link>>,

    /// Guards against a second dial starting while the first is still in its handshake, which
    /// discovery would otherwise trigger every beacon interval.
    dialling: Mutex<HashSet<Uuid>>,

    handlers: Handlers,
    cancel: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    shut_down: AtomicBool,
}

pub struct PeerMesh {
    inner: Arc<Inner>,
}

impl PeerMesh {
    pub const DEFAULT_PORT: u16 = 42100;

    /// `port` 0 binds an ephemeral port; [`Self::port`] says which.
    pub fn new(node_id: Uuid, identity: &NodeIdentity, pins: Arc<PinStore>, port: u16) -> io::Result<Self> {
        let client_config = tls::pinned_client_config(identity, Arc::clone(&pins)).map_err(io::Error::other)?;
        let server_config = tls::pinned_server_config(identity, Arc::clone(&pins)).map_err(io::Error::other)?;

        Ok(Self {
            inner: Arc::new(Inner {
                node_id,
                pins,
                requested_port: port,
                bound_port: AtomicU16::new(0),
                client_config,
                server_config,
                links: RwLock::new(HashMap::new()),
                dialling: Mutex::new(HashSet::new()),
                handlers: Handlers::default(),
                cancel: CancellationToken::new(),
                tasks: Mutex::new(Vec::new()),
                shut_down: AtomicBool::new(false),
            }),
        })
    }

    /// Called for every input batch received from any peer, on that link's receive task.
    pub fn on_input(&self, handler: impl Fn(Uuid, &[InputEvent], i64) + Send + Sync + 'static) {
        push(&self.inner.handlers.input, Arc::new(handler));
    }

    /// Called for every control payload received from any peer.
    pub fn on_control(&self, handler: impl Fn(Uuid, &[u8]) + Send + Sync + 'static) {
        push(&self.inner.handlers.control, Arc::new(handler));
    }

    /// Called when a peer connects (`true`) or disconnects (`false`).
    pub fn on_connection_changed(&self, handler: impl Fn(Uuid, bool) + Send + Sync + 'static) {
        push(&self.inner.handlers.connection, Arc::new(handler));
    }

    /// Called when a peer's outbound queue could not absorb an event that may not be dropped, or
    /// its link failed mid-send.
    ///
    /// The owner is expected to stop driving that peer. Continuing would mean discarding key
    /// transitions, and the one that goes missing is as likely to be a key-up as a key-down.
    pub fn on_send_stalled(&self, handler: impl Fn(Uuid, SendPumpStall) + Send + Sync + 'static) {
        push(&self.inner.handlers.stall, Arc::new(handler));
    }

    /// Binds the listener and starts accepting and heartbeating. Returns the port bound.
    pub async fn start(&self) -> io::Result<u16> {
        let listener = TcpListener::bind(("0.0.0.0", self.inner.requested_port)).await?;
        let port = listener.local_addr()?.port();
        self.inner.bound_port.store(port, Ordering::Release);

        let accept = tokio::spawn(accept_loop(Arc::clone(&self.inner), listener));
        let heartbeat = tokio::spawn(heartbeat_loop(Arc::downgrade(&self.inner), self.inner.cancel.clone()));

        let mut tasks = self.inner.tasks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        tasks.push(accept);
        tasks.push(heartbeat);

        log::info!("mesh listening on port {port}");
        Ok(port)
    }

    /// The port actually bound, once started.
    pub fn port(&self) -> u16 {
        self.inner.bound_port.load(Ordering::Acquire)
    }

    pub fn node_id(&self) -> Uuid {
        self.inner.node_id
    }

    /// Whether this node should dial `peer` rather than wait to be dialled.
    ///
    /// Both machines see each other's beacon at the same time, so without a rule they would dial
    /// simultaneously and end up with two links where one is needed. Lower node id dials: an
    /// arbitrary but total order, which both sides evaluate identically without negotiating.
    pub fn should_dial(&self, peer: Uuid) -> bool {
        self.inner.node_id < peer
    }

    /// Connects to a peer if not already connected or being dialled. Returns whether a link exists.
    pub async fn ensure_connected(&self, peer: Uuid, host: &str, port: u16) -> bool {
        let inner = &self.inner;

        if inner.has_link(peer) {
            return true;
        }

        if !inner.dialling.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(peer) {
            return false;
        }

        let result = PeerConnection::connect(host, port, inner.node_id, Arc::clone(&inner.client_config), &inner.pins).await;

        inner.dialling.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&peer);

        match result {
            Ok(connection) if connection.peer_id() == peer => {
                Inner::register(inner, connection);
                inner.has_link(peer)
            }
            Ok(connection) => {
                // The certificate resolved to a different machine than the beacon claimed. The pin
                // is authoritative; the beacon is unauthenticated and may be forged.
                log::warn!("dialled {peer} but its certificate identifies {}; dropping", connection.peer_id());
                connection.close().await;
                false
            }
            Err(error) => {
                log::info!("could not connect to {peer} at {host}:{port}: {error}");
                false
            }
        }
    }

    /// Hands one captured event to a peer's outbound queue.
    ///
    /// Returns as soon as it is queued: the capture side never waits on a socket. Returns `false`
    /// if the peer is not connected, or its queue was full of events that may not be dropped.
    pub fn try_send(&self, peer: Uuid, event: &InputEvent) -> bool {
        let links = self.inner.links.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        links.get(&peer).is_some_and(|link| link.pump.try_enqueue(event))
    }

    /// The outbound queue for a peer, for its counters.
    pub fn pump(&self, peer: Uuid) -> Option<Arc<PeerSendPump<PeerConnection>>> {
        self.inner.links.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&peer).map(|link| Arc::clone(&link.pump))
    }

    pub fn connection(&self, peer: Uuid) -> Option<Arc<PeerConnection>> {
        self.inner
            .links
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&peer)
            .map(|link| Arc::clone(&link.connection))
    }

    pub fn is_connected(&self, peer: Uuid) -> bool {
        self.inner.has_link(peer)
    }

    /// Sends a batch to one peer directly, bypassing its pump. Ignored if that peer is not
    /// connected: a send racing a disconnect is normal traffic, not an error.
    pub async fn send_input(&self, peer: Uuid, capture_us: i64, events: &[InputEvent]) {
        let Some(connection) = self.connection(peer) else {
            return;
        };

        if let Err(error) = connection.send_input(capture_us, events).await {
            log::info!("send to {peer} failed: {error}");
            Inner::drop_link(&self.inner, peer, Some(&connection)).await;
        }
    }

    /// Sends a control payload to every connected peer.
    pub async fn broadcast_control(&self, payload: &[u8]) {
        let connections: Vec<_> = self
            .inner
            .links
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|link| Arc::clone(&link.connection))
            .collect();

        for connection in connections {
            if let Err(error) = connection.send_control(payload).await {
                log::info!("broadcast to {} failed: {error}", connection.peer_id());
                Inner::drop_link(&self.inner, connection.peer_id(), Some(&connection)).await;
            }
        }
    }

    pub fn connected_peers(&self) -> Vec<Uuid> {
        self.inner.links.read().unwrap_or_else(|poisoned| poisoned.into_inner()).keys().copied().collect()
    }

    pub fn connection_count(&self) -> usize {
        self.inner.links.read().unwrap_or_else(|poisoned| poisoned.into_inner()).len()
    }

    /// Drops the link to a peer, as when it stops being trusted.
    pub async fn disconnect(&self, peer: Uuid) {
        Inner::drop_link(&self.inner, peer, None).await;
    }

    /// Closes every link and stops listening. Idempotent.
    pub async fn shutdown(&self) {
        if self.inner.shut_down.swap(true, Ordering::AcqRel) {
            return;
        }

        self.inner.cancel.cancel();

        for peer in self.connected_peers() {
            Inner::drop_link(&self.inner, peer, None).await;
        }

        let tasks = std::mem::take(&mut *self.inner.tasks.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        for task in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
    }
}

fn push<T>(handlers: &RwLock<Vec<T>>, handler: T) {
    handlers.write().unwrap_or_else(|poisoned| poisoned.into_inner()).push(handler);
}

impl Inner {
    fn has_link(&self, peer: Uuid) -> bool {
        self.links.read().unwrap_or_else(|poisoned| poisoned.into_inner()).contains_key(&peer)
    }

    fn register(this: &Arc<Self>, connection: Arc<PeerConnection>) {
        let peer = connection.peer_id();

        if this.shut_down.load(Ordering::Acquire) {
            tokio::spawn(async move { connection.close().await });
            return;
        }

        {
            let mut links = this.links.write().unwrap_or_else(|poisoned| poisoned.into_inner());

            // Simultaneous dials can still race despite should_dial, if each side learns of the
            // other through a different path. The newcomer loses, so both ends converge on one link.
            if links.contains_key(&peer) {
                drop(links);
                log::info!("already connected to {peer}; dropping the duplicate link");
                tokio::spawn(async move { connection.close().await });
                return;
            }

            let pump = Arc::new(PeerSendPump::new(
                Arc::clone(&connection),
                &peer.simple().to_string()[..8],
                send_pump::DEFAULT_CAPACITY,
            ));

            let weak = Arc::downgrade(this);
            pump.on_stalled(move |stall| {
                if let Some(inner) = weak.upgrade() {
                    each(&inner.handlers.stall, |handler| handler(peer, stall));
                }
            });

            pump.start();
            links.insert(peer, Link { connection: Arc::clone(&connection), pump });
        }

        log::info!("connected to peer {peer} at {}", connection.remote_address());
        each(&this.handlers.connection, |handler| handler(peer, true));

        let receiver = Arc::clone(this);
        let receiving = Arc::clone(&connection);
        let task = tokio::spawn(async move {
            let inner = &receiver;
            let result = receiving
                .receive_loop(
                    |id, events, capture_us| each(&inner.handlers.input, |handler| handler(id, events, capture_us)),
                    |id, payload| each(&inner.handlers.control, |handler| handler(id, payload)),
                )
                .await;

            if let Err(error) = result {
                log::info!("receive loop for {peer} ended: {error}");
            }

            Inner::drop_link(inner, peer, Some(&receiving)).await;
        });

        // Ordered after the receive loop is running, and sent as a burst. Measured: a lone ping sent
        // before the receive side had been scheduled came back with a round trip inflated by that
        // startup, and since the lowest round trip is the sample that wins, that one cold
        // measurement then governed every latency figure for the link -- reporting 2.4 ms on a path
        // that actually costs 98 us. Each ping carries its own send instant, so they are independent
        // samples, and by the second the path is warm.
        let seeding = Arc::clone(&connection);
        let seed = tokio::spawn(async move {
            for _ in 0..CLOCK_SEED_PINGS {
                if !seeding.send_ping().await {
                    return;
                }
            }
        });

        let mut tasks = this.tasks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
        tasks.push(seed);
    }

    /// Removes a peer's link. With `expected`, only if that is still the registered connection, so a
    /// stale receive loop ending cannot tear down a newer link to the same peer.
    async fn drop_link(this: &Arc<Self>, peer: Uuid, expected: Option<&Arc<PeerConnection>>) {
        let removed = {
            let mut links = this.links.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            let matches = match (links.get(&peer), expected) {
                (Some(link), Some(expected)) => Arc::ptr_eq(&link.connection, expected),
                (Some(_), None) => true,
                (None, _) => false,
            };

            if matches {
                links.remove(&peer)
            } else {
                None
            }
        };

        let Some(link) = removed else {
            if let Some(expected) = expected {
                expected.close().await;
            }
            return;
        };

        // The connection first: a pump part-way through a write is released by that write failing,
        // and its task then ends rather than waiting out the write timeout.
        link.connection.close().await;
        link.pump.stop();

        log::info!("disconnected from peer {peer}");
        each(&this.handlers.connection, |handler| handler(peer, false));
    }
}

async fn accept_loop(inner: Arc<Inner>, listener: TcpListener) {
    loop {
        let accepted = tokio::select! {
            _ = inner.cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };

        let (tcp, from) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                log::warn!("mesh accept failed: {error}");
                continue;
            }
        };

        let handshaking = Arc::clone(&inner);
        tokio::spawn(async move {
            let config = Arc::clone(&handshaking.server_config);
            let result = tokio::time::timeout(
                ACCEPT_TIMEOUT,
                PeerConnection::accept(tcp, handshaking.node_id, config, &handshaking.pins),
            )
            .await;

            match result {
                Ok(Ok(connection)) => Inner::register(&handshaking, connection),

                // An unpinned or malformed peer fails here. That is the mesh working, not a fault.
                Ok(Err(error)) => log::info!("rejected an inbound connection from {from}: {error}"),
                Err(_) => log::info!("an inbound connection from {from} did not complete its handshake"),
            }
        });
    }
}

/// Keeps every link's clock estimate fresh: fast at first, then slow, because after the first few
/// round trips the thing being tracked is crystal drift, which moves over minutes.
async fn heartbeat_loop(inner: Weak<Inner>, cancel: CancellationToken) {
    let mut ticks = 0;

    loop {
        let interval = if ticks < WARMUP_TICKS { WARMUP_INTERVAL } else { STEADY_INTERVAL };

        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }

        ticks += 1;

        let Some(inner) = inner.upgrade() else {
            return;
        };

        let connections: Vec<_> = inner
            .links
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|link| Arc::clone(&link.connection))
            .collect();

        drop(inner);

        for connection in connections {
            connection.send_ping().await;
        }
    }
}
