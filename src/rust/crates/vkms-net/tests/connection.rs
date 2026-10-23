//! Two peers over a real loopback socket, with real mutual TLS and real pinning. Only the network is
//! local; the handshake, the certificate validation and the framing are all genuine.

mod common;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::wait_for;
use tokio::net::TcpListener;
use uuid::Uuid;
use vkms_core::{clock, InputEvent, InputEventFlags, InputEventKind};
use vkms_net::{tls, NodeIdentity, PeerConnection, PinStore};

struct Fixture {
    alice: NodeIdentity,
    bob: NodeIdentity,
    alice_id: Uuid,
    bob_id: Uuid,
    alice_pins: Arc<PinStore>,
    bob_pins: Arc<PinStore>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            alice: NodeIdentity::generate("Alice").unwrap(),
            bob: NodeIdentity::generate("Bob").unwrap(),
            alice_id: Uuid::new_v4(),
            bob_id: Uuid::new_v4(),
            alice_pins: Arc::new(PinStore::new()),
            bob_pins: Arc::new(PinStore::new()),
        }
    }

    /// Pins each side to the other, as a completed pairing would.
    fn pin_each_other(&self) {
        self.alice_pins.pin(self.bob.thumbprint(), self.bob_id);
        self.bob_pins.pin(self.alice.thumbprint(), self.alice_id);
    }

    /// Alice dials, Bob accepts, on an ephemeral port.
    async fn connect(&self) -> io::Result<(Arc<PeerConnection>, Arc<PeerConnection>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();

        let client_config = tls::pinned_client_config(&self.alice, Arc::clone(&self.alice_pins)).unwrap();
        let server_config = tls::pinned_server_config(&self.bob, Arc::clone(&self.bob_pins)).unwrap();

        let server = async {
            let (tcp, _) = listener.accept().await?;
            PeerConnection::accept(tcp, self.bob_id, server_config, &self.bob_pins).await
        };

        let client = PeerConnection::connect("127.0.0.1", port, self.alice_id, client_config, &self.alice_pins);

        let (client, server) = tokio::join!(client, server);
        Ok((client?, server?))
    }
}

/// Runs a receive loop that records every event it sees.
fn record_input(connection: &Arc<PeerConnection>) -> Arc<Mutex<Vec<InputEvent>>> {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&received);
    let connection = Arc::clone(connection);

    tokio::spawn(async move {
        let _ = connection
            .receive_loop(
                |_, events, _| sink.lock().unwrap().extend_from_slice(events),
                |_, _| {},
            )
            .await;
    });

    received
}

#[tokio::test(flavor = "multi_thread")]
async fn mutually_pinned_peers_connect_and_identify_each_other() {
    let fixture = Fixture::new();
    fixture.pin_each_other();

    let (client, server) = fixture.connect().await.unwrap();

    // Each side resolved the other's identity from its pinned certificate, not from anything the
    // peer claimed about itself.
    assert_eq!(fixture.bob_id, client.peer_id());
    assert_eq!(fixture.alice_id, server.peer_id());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpinned_peer_is_refused() {
    // Without a pin there is no CA to fall back on, so the handshake must fail rather than accept a
    // valid-looking self-signed certificate. Bob trusts Alice; Alice does not trust Bob.
    let fixture = Fixture::new();
    fixture.bob_pins.pin(fixture.alice.thumbprint(), fixture.alice_id);

    assert!(fixture.connect().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_the_server_does_not_trust_is_refused_even_though_its_own_handshake_completed() {
    // TLS 1.3 completes the client's handshake before the server has judged the client certificate.
    // Only the hello exchange turns the server's refusal into a failed connection on the client.
    let fixture = Fixture::new();
    fixture.alice_pins.pin(fixture.bob.thumbprint(), fixture.bob_id);

    assert!(fixture.connect().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_with_no_certificate_pinned_at_all_is_refused() {
    assert!(Fixture::new().connect().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn input_events_travel_across_the_encrypted_link() {
    let fixture = Fixture::new();
    fixture.pin_each_other();
    let (client, server) = fixture.connect().await.unwrap();

    let received = record_input(&server);

    let batch = [
        InputEvent::mouse_move(100, 5, -5),
        InputEvent::key(100, 0x1E, InputEventFlags::NONE),
        InputEvent::mouse_button(100, 0, true),
    ];

    // Measured as a delta: the connection has already exchanged a hello frame.
    let frames_before = client.frames_sent();
    client.send_input(100, &batch).await.unwrap();

    assert!(wait_for(Duration::from_secs(10), || received.lock().unwrap().len() >= 3).await);

    let received = received.lock().unwrap();
    assert_eq!(InputEventKind::MOUSE_MOVE, received[0].kind);
    assert_eq!(5, received[0].x);
    assert_eq!(0x1E, received[1].code);
    assert!(received[2].is_up());

    assert_eq!(1, client.frames_sent() - frames_before);
    assert_eq!(3, client.events_sent());
}

#[tokio::test(flavor = "multi_thread")]
async fn many_batches_arrive_complete_and_in_order() {
    // Frames split and coalesce arbitrarily across TCP reads; ordering is what a key-up depends on.
    let fixture = Fixture::new();
    fixture.pin_each_other();
    let (client, server) = fixture.connect().await.unwrap();

    const BATCHES: i32 = 500;
    const PER_BATCH: i32 = 8;
    let received = record_input(&server);

    let mut sequence = 0;
    for batch in 0..BATCHES {
        let events: Vec<_> = (0..PER_BATCH)
            .map(|_| {
                sequence += 1;
                InputEvent::mouse_move(batch as i64, sequence - 1, 0)
            })
            .collect();

        client.send_input(batch as i64, &events).await.unwrap();
    }

    assert!(wait_for(Duration::from_secs(20), || received.lock().unwrap().len() >= (BATCHES * PER_BATCH) as usize).await);

    let received = received.lock().unwrap();
    for (index, event) in received.iter().enumerate() {
        assert_eq!(index as i32, event.x);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn control_payloads_travel_alongside_input() {
    let fixture = Fixture::new();
    fixture.pin_each_other();
    let (client, server) = fixture.connect().await.unwrap();

    let seen = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&seen);
    let receiving = Arc::clone(&server);
    tokio::spawn(async move {
        let _ = receiving.receive_loop(|_, _, _| {}, |_, payload| *sink.lock().unwrap() = Some(payload.to_vec())).await;
    });

    client.send_control(b"hello").await.unwrap();

    assert!(wait_for(Duration::from_secs(10), || seen.lock().unwrap().is_some()).await);
    assert_eq!(Some(b"hello".to_vec()), *seen.lock().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn latency_is_measured_on_receive() {
    let fixture = Fixture::new();
    fixture.pin_each_other();
    let (client, server) = fixture.connect().await.unwrap();

    let received = record_input(&server);

    // Microseconds, which is what the wire carries. Handing it raw ticks is exactly the mistake the
    // shared clock exists to prevent, and it reads as a plausible number.
    client.send_input(clock::microseconds(), &[InputEvent::mouse_move(0, 1, 1)]).await.unwrap();

    assert!(wait_for(Duration::from_secs(10), || !received.lock().unwrap().is_empty()).await);
    assert_eq!(1, server.latency().count());
    assert!(server.latency().max_microseconds() >= 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_one_end_ends_the_other_ends_receive_loop() {
    let fixture = Fixture::new();
    fixture.pin_each_other();
    let (client, server) = fixture.connect().await.unwrap();

    let receiving = Arc::clone(&server);
    let loop_task = tokio::spawn(async move { receiving.receive_loop(|_, _, _| {}, |_, _| {}).await });

    client.close().await;

    let result = common::within(Duration::from_secs(10), "the receive loop to end", loop_task).await;
    assert!(result.unwrap().is_ok(), "a clean close is not an error");
}
