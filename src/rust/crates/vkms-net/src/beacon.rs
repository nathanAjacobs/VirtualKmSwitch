//! Announces this machine on the LAN and listens for others.
//!
//! Multicast rather than broadcast: broadcast is filtered by many switches and ignored by most
//! wireless access points, so it fails exactly on the mixed wired/wireless networks this is meant
//! to work on. Pairing still accepts an address typed by hand for segments where multicast is
//! blocked too.
//!
//! The beacon carries no secret. A machine with its pairing window open advertises a one-way hash of
//! the code, so the machine the code is typed into can find it without the code itself ever being
//! sent anywhere.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Administratively scoped multicast, which routers do not forward off the local network.
pub const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);
pub const DEFAULT_PORT: u16 = 42099;

const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);

/// What a machine advertises about itself.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct BeaconMessage {
    pub node_id: Uuid,
    pub machine_name: String,

    /// Port this machine accepts mesh connections on.
    pub port: u16,

    /// This machine's certificate thumbprint. Informational only: unauthenticated, like everything
    /// else in a beacon, and never used to decide trust.
    pub thumbprint: String,

    /// Whether this machine is currently accepting a pairing, where, and for which code.
    pub pairing_open: bool,
    pub pairing_port: u16,

    /// A one-way hash of the pairing code, so the machine the code is typed into can tell which
    /// machine on the network is showing it.
    pub pairing_id: String,
}

pub type PeerSeenHandler = Arc<dyn Fn(&BeaconMessage, SocketAddr) + Send + Sync>;

struct Inner {
    advert: Mutex<Option<BeaconMessage>>,
    handlers: RwLock<Vec<PeerSeenHandler>>,
    sent: AtomicI64,
    received: AtomicI64,
}

pub struct DiscoveryBeacon {
    inner: Arc<Inner>,
    cancel: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl DiscoveryBeacon {
    /// Binds the listener and starts announcing (once [`Self::advertise`] has said what).
    pub async fn start(port: u16) -> io::Result<Self> {
        let listener = bind_listener(port)?;
        let sender = UdpSocket::bind(("0.0.0.0", 0)).await?;

        let inner = Arc::new(Inner {
            advert: Mutex::new(None),
            handlers: RwLock::new(Vec::new()),
            sent: AtomicI64::new(0),
            received: AtomicI64::new(0),
        });

        let cancel = CancellationToken::new();
        let tasks = vec![
            tokio::spawn(listen(Arc::clone(&inner), listener, cancel.clone())),
            tokio::spawn(announce(Arc::clone(&inner), sender, port, cancel.clone())),
        ];

        Ok(Self { inner, cancel, tasks: Mutex::new(tasks) })
    }

    /// Sets what this machine advertises, taking effect from the next announcement.
    pub fn advertise(&self, message: BeaconMessage) {
        *self.inner.advert.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
    }

    /// Called for every beacon received, this machine's own included.
    pub fn on_peer_seen(&self, handler: impl Fn(&BeaconMessage, SocketAddr) + Send + Sync + 'static) {
        self.inner.handlers.write().unwrap_or_else(|poisoned| poisoned.into_inner()).push(Arc::new(handler));
    }

    pub fn beacons_sent(&self) -> i64 {
        self.inner.sent.load(Ordering::Relaxed)
    }

    pub fn beacons_received(&self) -> i64 {
        self.inner.received.load(Ordering::Relaxed)
    }

    pub async fn shutdown(&self) {
        self.cancel.cancel();

        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        for task in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
    }
}

impl Drop for DiscoveryBeacon {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn bind_listener(port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    // Several instances on one machine, and an instance beside a debugging tool, must both work;
    // without this the second binder simply fails.
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into())?;
    socket.join_multicast_v4(&MULTICAST_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_nonblocking(true)?;

    UdpSocket::from_std(socket.into())
}

async fn announce(inner: Arc<Inner>, sender: UdpSocket, port: u16, cancel: CancellationToken) {
    let destination = SocketAddr::V4(SocketAddrV4::new(MULTICAST_GROUP, port));

    loop {
        let advert = inner.advert.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();

        if let Some(message) = advert {
            match serde_json::to_vec(&message) {
                Ok(payload) => match sender.send_to(&payload, destination).await {
                    Ok(_) => {
                        inner.sent.fetch_add(1, Ordering::Relaxed);
                    }

                    // A transient network change should not stop announcing for good.
                    Err(error) => log::debug!("beacon announce failed: {error}"),
                },
                Err(error) => log::warn!("could not encode the beacon: {error}"),
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(ANNOUNCE_INTERVAL) => {}
        }
    }
}

async fn listen(inner: Arc<Inner>, socket: UdpSocket, cancel: CancellationToken) {
    let mut buffer = vec![0u8; 4096];

    loop {
        let received = tokio::select! {
            _ = cancel.cancelled() => return,
            received = socket.recv_from(&mut buffer) => received,
        };

        let (length, from) = match received {
            Ok(received) => received,
            Err(error) => {
                log::debug!("beacon listen failed: {error}");
                continue;
            }
        };

        inner.received.fetch_add(1, Ordering::Relaxed);

        // Anything on this port is unauthenticated and arbitrary, so a malformed datagram is ignored
        // rather than allowed to take the listener down.
        let Ok(message) = serde_json::from_slice::<BeaconMessage>(&buffer[..length]) else {
            continue;
        };

        if message.node_id.is_nil() {
            continue;
        }

        for handler in inner.handlers.read().unwrap_or_else(|poisoned| poisoned.into_inner()).iter() {
            handler(&message, from);
        }
    }
}
