//! Two complete meshes on loopback: real listeners, real dialling, real mutual TLS, real pinning.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{wait_for, within};
use uuid::Uuid;
use vkms_core::{clock, InputEvent, InputEventFlags};
use vkms_net::{NodeIdentity, PeerMesh, PinStore};

struct TwoNodes {
    alice_id: Uuid,
    bob_id: Uuid,
    alice_pins: Arc<PinStore>,
    bob_pins: Arc<PinStore>,
    alice: PeerMesh,
    bob: PeerMesh,
}

impl TwoNodes {
    fn new() -> Self {
        let alice_identity = NodeIdentity::generate("MeshAlice").unwrap();
        let bob_identity = NodeIdentity::generate("MeshBob").unwrap();

        // Ordered so Alice is always the dialler, matching the lower-id-dials rule.
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let (alice_id, bob_id) = if first < second { (first, second) } else { (second, first) };

        let alice_pins = Arc::new(PinStore::new());
        let bob_pins = Arc::new(PinStore::new());
        alice_pins.pin(bob_identity.thumbprint(), bob_id);
        bob_pins.pin(alice_identity.thumbprint(), alice_id);

        Self {
            alice: PeerMesh::new(alice_id, &alice_identity, Arc::clone(&alice_pins), 0).unwrap(),
            bob: PeerMesh::new(bob_id, &bob_identity, Arc::clone(&bob_pins), 0).unwrap(),
            alice_id,
            bob_id,
            alice_pins,
            bob_pins,
        }
    }

    async fn connect(&self) -> bool {
        self.alice.start().await.unwrap();
        let bob_port = self.bob.start().await.unwrap();
        self.alice.ensure_connected(self.bob_id, "127.0.0.1", bob_port).await
    }

    async fn shutdown(&self) {
        self.alice.shutdown().await;
        self.bob.shutdown().await;
    }
}

#[test]
fn dialling_is_decided_by_node_order_so_both_sides_agree() {
    // Both peers see each other simultaneously. Without a total order they would dial at once and
    // build two links where one is wanted; each side evaluates this identically.
    let low = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let high = Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
    let identity = NodeIdentity::generate("Order").unwrap();

    let low_mesh = PeerMesh::new(low, &identity, Arc::new(PinStore::new()), 0).unwrap();
    let high_mesh = PeerMesh::new(high, &identity, Arc::new(PinStore::new()), 0).unwrap();

    assert!(low_mesh.should_dial(high));
    assert!(!high_mesh.should_dial(low));
}

#[tokio::test(flavor = "multi_thread")]
async fn peers_connect_and_both_sides_see_the_link() {
    let nodes = TwoNodes::new();
    assert!(nodes.connect().await);

    assert!(wait_for(Duration::from_secs(10), || nodes.bob.connection_count() > 0).await);
    assert!(nodes.alice.connected_peers().contains(&nodes.bob_id));
    assert!(nodes.bob.connected_peers().contains(&nodes.alice_id));

    nodes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn input_flows_between_meshes() {
    let nodes = TwoNodes::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&received);
    nodes.bob.on_input(move |_, events, _| sink.lock().unwrap().extend_from_slice(events));

    assert!(nodes.connect().await);
    assert!(wait_for(Duration::from_secs(10), || nodes.bob.connection_count() > 0).await);

    nodes.alice.send_input(nodes.bob_id, 42, &[InputEvent::key(42, 0x1E, InputEventFlags::NONE)]).await;

    assert!(wait_for(Duration::from_secs(10), || !received.lock().unwrap().is_empty()).await);
    assert_eq!(0x1E, received.lock().unwrap()[0].code);

    nodes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sending_to_an_unknown_peer_is_ignored_rather_than_failing() {
    // Peers disappear constantly -- sleep, reboot, network change. A send racing a disconnect is
    // normal traffic, not an error worth propagating into the input path.
    let nodes = TwoNodes::new();
    nodes.alice.start().await.unwrap();

    nodes.alice.send_input(Uuid::new_v4(), 1, &[InputEvent::mouse_move(1, 1, 1)]).await;
    assert!(!nodes.alice.try_send(Uuid::new_v4(), &InputEvent::mouse_move(1, 1, 1)));

    nodes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpinned_peer_cannot_join_the_mesh() {
    let nodes = TwoNodes::new();

    // Bob forgets Alice, as removing her would do.
    nodes.bob_pins.clear();

    nodes.alice.start().await.unwrap();
    let bob_port = nodes.bob.start().await.unwrap();

    assert!(!nodes.alice.ensure_connected(nodes.bob_id, "127.0.0.1", bob_port).await);
    assert!(nodes.alice.connected_peers().is_empty());

    nodes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_beacon_claiming_a_different_identity_than_the_certificate_is_not_believed() {
    // The beacon is unauthenticated; the pin is authoritative. Dialling "someone else" at Bob's
    // address must not produce a link filed under the wrong machine.
    let nodes = TwoNodes::new();
    nodes.alice.start().await.unwrap();
    let bob_port = nodes.bob.start().await.unwrap();

    let impostor = Uuid::new_v4();
    nodes.alice_pins.pin("00", impostor);

    assert!(!nodes.alice.ensure_connected(impostor, "127.0.0.1", bob_port).await);
    assert!(!nodes.alice.is_connected(impostor));

    nodes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_eight_kilohertz_stream_stays_inside_the_latency_budget_and_keeps_its_order() {
    // The budget is p99 at 2 ms added. On loopback the wire costs nothing, so what is measured is
    // the queue, the coalescer, the codec and TLS -- and it should sit far below the budget.
    //
    // Pacing matters as much as the count. An unpaced loop produces events faster than any mouse in
    // existence, the queue backs up, and the figure that comes out measures the test harness.
    let nodes = TwoNodes::new();

    const EVENTS: usize = 2_000;
    const PERIOD_US: i64 = 125; // 8 kHz

    let received = Arc::new(Mutex::new(Vec::with_capacity(EVENTS)));
    let direct = Arc::new(Mutex::new(Vec::with_capacity(EVENTS)));

    // Measured a second way, straight from the batch timestamp with no clock offset applied. On
    // loopback both ends read the same clock, so the two must agree; a biased clock estimate would
    // make the reported figures confidently wrong rather than merely noisy.
    let (sink, direct_sink) = (Arc::clone(&received), Arc::clone(&direct));
    nodes.bob.on_input(move |_, events, capture_us| {
        direct_sink.lock().unwrap().push(clock::microseconds() - capture_us);
        sink.lock().unwrap().extend(events.iter().map(|e| e.code));
    });

    assert!(nodes.connect().await);
    assert!(wait_for(Duration::from_secs(10), || nodes.bob.connection_count() > 0).await);

    // Key events, not motion: motion is allowed to merge, so counting it would assert on how the
    // scheduler happened to interleave. Sent from a plain thread, as the router does.
    let alice = &nodes.alice;
    let bob_id = nodes.bob_id;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut next = clock::microseconds();
            for i in 0..EVENTS {
                while clock::microseconds() < next {
                    std::hint::spin_loop();
                }
                next += PERIOD_US;

                assert!(alice.try_send(bob_id, &InputEvent::key(clock::ticks(), i as u16, InputEventFlags::NONE)));
            }
        });
    });

    assert!(wait_for(Duration::from_secs(30), || received.lock().unwrap().len() >= EVENTS).await);

    let connection = nodes.bob.connection(nodes.alice_id).unwrap();
    let pump = nodes.alice.pump(nodes.bob_id).unwrap();
    let p99 = connection.latency().percentile(0.99);

    // Nothing reordered: a key-up that overtook its key-down would leave the key held.
    let codes = received.lock().unwrap().clone();
    assert_eq!((0..EVENTS as u16).collect::<Vec<_>>(), codes);
    assert_eq!(0, pump.moves_dropped());

    assert!(p99 < 2_000, "p99 was {p99}us, over the 2ms budget. {}", connection.latency().summary());

    let mut direct = direct.lock().unwrap().clone();
    direct.sort_unstable();
    let direct_median = direct[direct.len() / 2];
    let direct_p99 = direct[direct.len() * 99 / 100];
    let direct_p999 = direct[direct.len() * 999 / 1000];
    let reported_median = connection.latency().percentile(0.50);
    assert!(
        reported_median <= direct_median + 500,
        "reported p50 {reported_median}us disagrees with the direct measurement {direct_median}us"
    );

    assert!(connection.clock().has_sample(), "the link reported latency without ever synchronising its clock");

    eprintln!(
        "8 kHz loopback: {} | direct p50 {direct_median}us p99 {direct_p99}us p999 {direct_p999}us | clock offset {}us | largest batch {}",
        connection.latency().summary(),
        connection.clock().offset_microseconds(),
        pump.largest_batch()
    );

    nodes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clock_synchronisation_converges_without_being_asked_to() {
    let nodes = TwoNodes::new();
    assert!(nodes.connect().await);
    assert!(wait_for(Duration::from_secs(10), || nodes.bob.connection_count() > 0).await);

    let from_alice = nodes.bob.connection(nodes.alice_id).unwrap();
    let from_bob = nodes.alice.connection(nodes.bob_id).unwrap();

    assert!(wait_for(Duration::from_secs(10), || from_alice.clock().has_sample() && from_bob.clock().has_sample()).await);

    // Both ends of a loopback link read the same clock, so the true offset is zero. Anything else
    // is the estimator inventing a difference, which on two real machines reads as latency.
    assert!((-2_000..=2_000).contains(&from_alice.clock().offset_microseconds()));
    assert!((-2_000..=2_000).contains(&from_bob.clock().offset_microseconds()));
    assert!((0..=100_000).contains(&from_alice.clock().round_trip_microseconds()));

    nodes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnection_is_reported_to_both_sides() {
    let nodes = TwoNodes::new();
    let bob_saw_disconnect = Arc::new(Mutex::new(false));
    let record = Arc::clone(&bob_saw_disconnect);
    nodes.bob.on_connection_changed(move |_, connected| {
        if !connected {
            *record.lock().unwrap() = true;
        }
    });

    assert!(nodes.connect().await);
    assert!(wait_for(Duration::from_secs(10), || nodes.bob.connection_count() > 0).await);

    nodes.alice.shutdown().await;

    assert!(wait_for(Duration::from_secs(10), || *bob_saw_disconnect.lock().unwrap()).await);
    assert!(nodes.bob.connected_peers().is_empty());

    nodes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnecting_a_peer_closes_its_link_on_both_ends() {
    // What removing a paired machine does: revoking its pin stops new links, and this ends the one
    // already open.
    let nodes = TwoNodes::new();
    assert!(nodes.connect().await);
    assert!(wait_for(Duration::from_secs(10), || nodes.bob.connection_count() > 0).await);

    within(Duration::from_secs(10), "the disconnect", nodes.alice.disconnect(nodes.bob_id)).await;

    assert!(!nodes.alice.is_connected(nodes.bob_id));
    assert!(wait_for(Duration::from_secs(10), || nodes.bob.connection_count() == 0).await);

    nodes.shutdown().await;
}
