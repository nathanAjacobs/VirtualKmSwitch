//! One authenticated, encrypted link to another peer.
//!
//! Both directions carry input, because either end may be driving the other: there is no client and
//! server here, only peers.
//!
//! TCP rather than UDP. Loss on a LAN is rare, but a key-up overtaking or replacing its key-down
//! leaves a modifier stuck down on a remote machine with no way to clear it from there -- a far
//! worse outcome than the occasional retransmit.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::{ClientConfig, ServerConfig};
use tokio::io::{split, AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use vkms_core::{clock, InputEvent};

use crate::clock_sync::{self, ClockSync, HeartbeatKind};
use crate::codec::{self, FrameRead, FrameType};
use crate::latency::LatencyTracker;
use crate::pins::PinStore;
use crate::send_pump::InputSink;
use crate::tls;

type Stream = TlsStream<TcpStream>;

/// Sized so a full drain of the capture ring fits in one write. Splitting a batch across writes
/// would put a second TLS record -- and a second syscall -- on the latency path for no reason.
const SEND_BUFFER_SIZE: usize = 64 * 1024;
const RECEIVE_BUFFER_SIZE: usize = 64 * 1024;

/// node id (16) + clock sample (8).
const HELLO_PAYLOAD_SIZE: usize = 24;

/// A peer that has accepted the connection replies at once. Anything slower is a peer that rejected
/// us and is about to close, or one too unhealthy to drive a machine with.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// An input write that cannot complete in this long is a link that has stopped draining.
/// Giving up lets the pump report it rather than hang behind a dead socket forever.
const INPUT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

struct Writer {
    half: WriteHalf<Stream>,
    buffer: Box<[u8]>,
}

struct Reader {
    half: ReadHalf<Stream>,
    buffer: Box<[u8]>,
    filled: usize,
}

pub struct PeerConnection {
    peer_id: Uuid,
    remote: SocketAddr,

    // Per-connection buffers rather than a pool: this path runs at kilohertz rates, where pool
    // contention and return-path mistakes cost more than the memory saved.
    writer: tokio::sync::Mutex<Writer>,
    reader: Mutex<Option<Reader>>,

    closed: CancellationToken,

    latency: LatencyTracker,
    clock: ClockSync,

    frames_sent: AtomicI64,
    frames_received: AtomicI64,
    events_sent: AtomicI64,
    events_received: AtomicI64,
    heartbeats_sent: AtomicI64,
}

fn protocol_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl PeerConnection {
    /// Dials a peer and authenticates as the client half of mutual TLS.
    pub async fn connect(
        host: &str,
        port: u16,
        local_id: Uuid,
        config: Arc<ClientConfig>,
        pins: &PinStore,
    ) -> io::Result<Arc<Self>> {
        let tcp = TcpStream::connect((host, port)).await?;

        // Nagle would batch small frames into larger ones, which is the opposite of what an input
        // stream wants: it trades exactly the latency this system is built to minimise.
        tcp.set_nodelay(true)?;
        let remote = tcp.peer_addr()?;

        let tls = TlsConnector::from(config).connect(tls::server_name(), tcp).await?;
        let peer_id = resolve(tls::peer_thumbprint(tls.get_ref().1), pins)?;

        Self::establish(Stream::from(tls), peer_id, local_id, remote).await
    }

    /// Authenticates an inbound connection as the server half of mutual TLS.
    pub async fn accept(
        tcp: TcpStream,
        local_id: Uuid,
        config: Arc<ServerConfig>,
        pins: &PinStore,
    ) -> io::Result<Arc<Self>> {
        tcp.set_nodelay(true)?;
        let remote = tcp.peer_addr()?;

        let tls = TlsAcceptor::from(config).accept(tcp).await?;
        let peer_id = resolve(tls::peer_thumbprint(tls.get_ref().1), pins)?;

        Self::establish(Stream::from(tls), peer_id, local_id, remote).await
    }

    async fn establish(mut stream: Stream, peer_id: Uuid, local_id: Uuid, remote: SocketAddr) -> io::Result<Arc<Self>> {
        let (buffer, filled) = match tokio::time::timeout(HELLO_TIMEOUT, exchange_hello(&mut stream, local_id, peer_id)).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "peer {peer_id} did not complete the handshake within {}s; it most likely rejected this \
                         node's certificate",
                        HELLO_TIMEOUT.as_secs()
                    ),
                ))
            }
        };

        let (read_half, write_half) = split(stream);

        Ok(Arc::new(Self {
            peer_id,
            remote,
            writer: tokio::sync::Mutex::new(Writer {
                half: write_half,
                buffer: vec![0u8; SEND_BUFFER_SIZE].into_boxed_slice(),
            }),
            reader: Mutex::new(Some(Reader { half: read_half, buffer, filled })),
            closed: CancellationToken::new(),
            latency: LatencyTracker::new(),
            clock: ClockSync::new(),
            frames_sent: AtomicI64::new(0),
            frames_received: AtomicI64::new(0),
            events_sent: AtomicI64::new(0),
            events_received: AtomicI64::new(0),
            heartbeats_sent: AtomicI64::new(0),
        }))
    }

    /// The machine this link is authenticated to, resolved from its pinned certificate.
    pub fn peer_id(&self) -> Uuid {
        self.peer_id
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.remote
    }

    /// Added latency of received batches, measured through [`Self::clock`].
    pub fn latency(&self) -> &LatencyTracker {
        &self.latency
    }

    /// The running estimate of this peer's clock relative to ours. A link with no completed
    /// heartbeat reports latency against a zero offset; [`ClockSync::has_sample`] says which.
    pub fn clock(&self) -> &ClockSync {
        &self.clock
    }

    pub fn frames_sent(&self) -> i64 {
        self.frames_sent.load(Ordering::Relaxed)
    }

    pub fn frames_received(&self) -> i64 {
        self.frames_received.load(Ordering::Relaxed)
    }

    pub fn events_sent(&self) -> i64 {
        self.events_sent.load(Ordering::Relaxed)
    }

    pub fn events_received(&self) -> i64 {
        self.events_received.load(Ordering::Relaxed)
    }

    pub fn heartbeats_sent(&self) -> i64 {
        self.heartbeats_sent.load(Ordering::Relaxed)
    }

    pub fn is_connected(&self) -> bool {
        !self.closed.is_cancelled()
    }

    /// Sends a batch of input events as a single frame.
    ///
    /// One write per batch, never per event: TLS adds record framing and a syscall to every write,
    /// and at kilohertz rates that overhead dwarfs the twelve bytes an event actually costs.
    pub async fn send_input(&self, capture_us: i64, events: &[InputEvent]) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let mut writer = self.writer.lock().await;
        let Writer { half, buffer } = &mut *writer;

        let Some(written) = codec::write_input_batch(buffer, capture_us, events) else {
            log::warn!("batch of {} events does not fit in one frame; dropping", events.len());
            return Ok(());
        };

        half.write_all(&buffer[..written]).await?;
        half.flush().await?;

        self.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.events_sent.fetch_add(events.len() as i64, Ordering::Relaxed);
        Ok(())
    }

    /// Sends a control payload.
    pub async fn send_control(&self, payload: &[u8]) -> io::Result<()> {
        self.write_frame(FrameType::Control, payload).await
    }

    /// Starts one clock-synchronisation round trip.
    ///
    /// Cheap enough to repeat: two small frames, and the estimate only improves with more samples
    /// because the lowest-round-trip one is the one kept.
    pub async fn send_ping(&self) -> bool {
        let mut payload = [0u8; clock_sync::PAYLOAD_SIZE];
        clock_sync::write_ping(&mut payload, clock::microseconds());

        let sent = self.write_frame(FrameType::Heartbeat, &payload).await.is_ok();
        if sent {
            self.heartbeats_sent.fetch_add(1, Ordering::Relaxed);
        }

        sent
    }

    async fn write_frame(&self, frame_type: FrameType, payload: &[u8]) -> io::Result<()> {
        let mut writer = self.writer.lock().await;
        let Writer { half, buffer } = &mut *writer;

        let written = codec::write_control(buffer, frame_type, payload)
            .ok_or_else(|| protocol_error(format!("a payload of {} bytes is too large to frame", payload.len())))?;

        half.write_all(&buffer[..written]).await?;
        half.flush().await?;
        self.frames_sent.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Reads frames until the connection closes, is closed, or misbehaves.
    ///
    /// Heartbeats are answered here rather than handed upward. A round trip that waited behind
    /// whatever the owner does with a control message would measure that wait as network delay.
    pub async fn receive_loop<I, C>(&self, mut on_input: I, mut on_control: C) -> io::Result<()>
    where
        I: FnMut(Uuid, &[InputEvent], i64) + Send,
        C: FnMut(Uuid, &[u8]) + Send,
    {
        let taken = self.reader.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        let mut reader = taken.ok_or_else(|| io::Error::other("the receive loop is already running"))?;

        let mut decoded = vec![InputEvent::default(); codec::MAX_EVENTS_PER_BATCH];
        let mut pong = [0u8; clock_sync::PAYLOAD_SIZE];

        loop {
            // Decode every complete frame already buffered -- including any that arrived alongside
            // the hello -- then compact what remains.
            let mut consumed_total = 0;

            loop {
                let (frame_type, payload, consumed) = match codec::read_frame(&reader.buffer[consumed_total..reader.filled]) {
                    FrameRead::Complete { frame_type, payload, consumed } => (frame_type, payload, consumed),
                    FrameRead::Incomplete => break,
                    FrameRead::Malformed => {
                        return Err(protocol_error(format!("peer {} sent a malformed frame", self.peer_id)))
                    }
                };

                self.frames_received.fetch_add(1, Ordering::Relaxed);

                match frame_type {
                    FrameType::InputBatch => {
                        let (count, capture_us) = codec::read_input_batch(payload, &mut decoded)
                            .ok_or_else(|| protocol_error(format!("peer {} sent a malformed batch", self.peer_id)))?;

                        self.events_received.fetch_add(count as i64, Ordering::Relaxed);
                        self.latency.record_since(capture_us, self.clock.offset_microseconds());
                        on_input(self.peer_id, &decoded[..count], capture_us);
                    }

                    FrameType::Heartbeat => {
                        // Taken before anything else, so our own processing is not counted as wire time.
                        let received_at = clock::microseconds();

                        match clock_sync::read(payload) {
                            Some((HeartbeatKind::Ping, t1, _, _)) => {
                                clock_sync::write_pong(&mut pong, t1, received_at, clock::microseconds());
                                let _ = self.write_frame(FrameType::Heartbeat, &pong).await;
                            }
                            Some((HeartbeatKind::Pong, t1, t2, t3)) => self.clock.record(t1, t2, t3, received_at),
                            None => {}
                        }
                    }

                    FrameType::Control => on_control(self.peer_id, payload),

                    // A second hello is meaningless; the identity was settled by the first.
                    FrameType::Hello => {}
                }

                consumed_total += consumed;
            }

            if consumed_total > 0 {
                // Move the partial frame to the front so the next read appends to it.
                reader.buffer.copy_within(consumed_total..reader.filled, 0);
                reader.filled -= consumed_total;
            } else if reader.filled == reader.buffer.len() {
                // A full buffer holding no complete frame cannot be a frame at all. The length check
                // should have caught this; it is a last resort.
                return Err(protocol_error(format!("peer {} overran the receive buffer", self.peer_id)));
            }

            let Reader { half, buffer, filled } = &mut reader;
            let read = tokio::select! {
                _ = self.closed.cancelled() => return Ok(()),
                read = half.read(&mut buffer[*filled..]) => read?,
            };

            if read == 0 {
                log::info!("peer {} closed the connection", self.peer_id);
                return Ok(());
            }

            *filled += read;
        }
    }

    /// Closes the link: stops the receive loop and sends a TLS close to the far end.
    pub async fn close(&self) {
        self.closed.cancel();

        // Bounded, because a write stuck behind a dead socket holds the writer lock.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            let mut writer = self.writer.lock().await;
            let _ = writer.half.shutdown().await;
        })
        .await;
    }
}

impl InputSink for PeerConnection {
    /// Sends a batch from a pump's task, giving up on a link that has stopped draining.
    async fn try_send_input(&self, capture_us: i64, events: &[InputEvent]) -> bool {
        if self.closed.is_cancelled() {
            return false;
        }

        match tokio::time::timeout(INPUT_WRITE_TIMEOUT, self.send_input(capture_us, events)).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                log::warn!("send to {} failed: {error}", self.peer_id);
                false
            }
            Err(_) => {
                log::warn!("send to {} stalled for {}s", self.peer_id, INPUT_WRITE_TIMEOUT.as_secs());
                false
            }
        }
    }
}

fn resolve(thumbprint: Option<String>, pins: &PinStore) -> io::Result<Uuid> {
    let thumbprint = thumbprint.ok_or_else(|| protocol_error("the peer presented no certificate"))?;

    // The verifier already required a pin; this only recovers which machine it belongs to. A pin
    // revoked in between is a refusal, not a race to be resolved in the peer's favour.
    pins.resolve(&thumbprint)
        .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "the peer's certificate is not trusted"))
}

/// Confirms the peer actually accepted this connection.
///
/// TLS 1.3 sends client credentials after the client's handshake completes, so a client whose
/// certificate the server refuses still reaches an "authenticated" state locally. Only a successful
/// round trip proves the link is usable in both directions.
async fn exchange_hello(stream: &mut Stream, local_id: Uuid, peer_id: Uuid) -> io::Result<(Box<[u8]>, usize)> {
    let mut payload = [0u8; HELLO_PAYLOAD_SIZE];
    payload[..16].copy_from_slice(&local_id.to_bytes_le());

    // Microseconds, never raw ticks: tick frequency is a property of the machine.
    payload[16..].copy_from_slice(&clock::microseconds().to_le_bytes());

    let mut frame = [0u8; codec::HEADER_SIZE + HELLO_PAYLOAD_SIZE];
    codec::write_control(&mut frame, FrameType::Hello, &payload).expect("a hello always fits");
    stream.write_all(&frame).await?;
    stream.flush().await?;

    // Read until a complete hello arrives. Anything else at this point is a protocol violation,
    // since neither side sends input before the exchange completes.
    let mut buffer = vec![0u8; RECEIVE_BUFFER_SIZE].into_boxed_slice();
    let mut filled = 0;

    loop {
        let read = stream.read(&mut buffer[filled..]).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the peer closed the connection before completing the handshake",
            ));
        }

        filled += read;

        match codec::read_frame(&buffer[..filled]) {
            FrameRead::Incomplete => continue,

            FrameRead::Complete { frame_type: FrameType::Hello, payload, consumed } if payload.len() == HELLO_PAYLOAD_SIZE => {
                let claimed = Uuid::from_bytes_le(payload[..16].try_into().expect("sixteen bytes"));

                // The pinned certificate is the identity; the hello merely has to agree with it. A
                // peer that names a different machine is misconfigured or lying.
                if claimed != peer_id {
                    return Err(protocol_error(format!(
                        "the peer's certificate identifies {peer_id} but it announced {claimed}"
                    )));
                }

                buffer.copy_within(consumed..filled, 0);
                return Ok((buffer, filled - consumed));
            }

            _ => return Err(protocol_error("the peer did not send a valid hello")),
        }
    }
}
