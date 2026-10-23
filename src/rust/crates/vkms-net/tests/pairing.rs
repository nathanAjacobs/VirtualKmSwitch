//! Pairing over real TLS: the only place the code is ever used, and the only moment a machine will
//! talk to one it does not yet trust.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::wait_for;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use uuid::Uuid;
use vkms_net::pairing::{self, EnrollmentOutcome, EnrollmentResult, PairingCode, PairingIdentity, PairingWindow};
use vkms_net::{tls, NodeIdentity};

const CODE: &str = "ABCD-EFGH-JKLM-NPQR";

struct Fixture {
    accepter: NodeIdentity,
    joiner: NodeIdentity,
    accepter_id: Uuid,
    joiner_id: Uuid,
    accepted: Arc<Mutex<Vec<EnrollmentOutcome>>>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            accepter: NodeIdentity::generate("PairAccepter").unwrap(),
            joiner: NodeIdentity::generate("PairJoiner").unwrap(),
            accepter_id: Uuid::new_v4(),
            joiner_id: Uuid::new_v4(),
            accepted: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn open(&self, duration: Duration) -> PairingWindow {
        let record = Arc::clone(&self.accepted);
        PairingWindow::open(
            PairingIdentity {
                node_id: self.accepter_id,
                display_name: "Desk".into(),
                code: PairingCode::normalize(CODE),
                mesh_port: 4242,
            },
            &self.accepter,
            duration,
            0,
            Arc::new(move |outcome| record.lock().unwrap().push(outcome.clone())),
        )
        .await
        .unwrap()
    }

    async fn join(&self, port: u16, code: &str) -> std::io::Result<EnrollmentOutcome> {
        pairing::join("127.0.0.1", port, code, self.joiner_id, &self.joiner, "Laptop", 4343).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_machines_holding_the_same_code_pair_with_each_other() {
    let fixture = Fixture::new();
    let window = fixture.open(Duration::from_secs(30)).await;

    let joiner_saw = fixture.join(window.port(), CODE).await.unwrap();
    assert!(wait_for(Duration::from_secs(15), || !fixture.accepted.lock().unwrap().is_empty()).await);
    let accepter_saw = fixture.accepted.lock().unwrap()[0].clone();

    assert!(joiner_saw.accepted(), "{:?}", joiner_saw.detail);
    assert!(accepter_saw.accepted(), "{:?}", accepter_saw.detail);

    // Each side comes away with the other's identity and the exact certificate it will pin.
    assert_eq!(fixture.accepter_id, joiner_saw.peer_node_id);
    assert_eq!(fixture.accepter.thumbprint(), joiner_saw.peer_thumbprint);
    assert_eq!("Desk", joiner_saw.peer_display_name);
    assert_eq!(4242, joiner_saw.peer_mesh_port, "each side learns where to reach the other");

    assert_eq!(fixture.joiner_id, accepter_saw.peer_node_id);
    assert_eq!(fixture.joiner.thumbprint(), accepter_saw.peer_thumbprint);
    assert_eq!("Laptop", accepter_saw.peer_display_name);
    assert_eq!(4343, accepter_saw.peer_mesh_port);
    assert!(accepter_saw.peer_address.is_some(), "the accepter needs to know where to dial back");

    window.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_code_is_refused() {
    let fixture = Fixture::new();
    let window = fixture.open(Duration::from_secs(30)).await;

    let outcome = fixture.join(window.port(), "ZZZZ-ZZZZ-ZZZZ-ZZZZ").await.unwrap();

    assert!(!outcome.accepted());
    assert_eq!(EnrollmentResult::WrongKey, outcome.result);

    // And the machine showing the code did not record anyone either.
    assert!(fixture.accepted.lock().unwrap().iter().all(|attempt| !attempt.accepted()));
    window.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn transcription_differences_in_the_typed_code_still_pair() {
    let fixture = Fixture::new();
    let window = fixture.open(Duration::from_secs(30)).await;

    let outcome = fixture.join(window.port(), "  abcd efgh JKLM-npqr ").await.unwrap();

    assert!(outcome.accepted(), "{:?}", outcome.detail);
    window.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_intercepted_connection_is_refused_even_with_the_right_code() {
    // The property the thumbprint binding exists for. The relay knows neither private key; it only
    // re-terminates TLS, which is exactly what an attacker on the same network can do. It relays both
    // proofs verbatim and they still do not verify.
    let fixture = Fixture::new();
    let window = fixture.open(Duration::from_secs(30)).await;
    let relay_port = start_intercepting_relay(window.port()).await;

    let outcome = fixture.join(relay_port, CODE).await.unwrap();

    assert!(!outcome.accepted());

    // Named for what it is. "Wrong code" here would send the user off to retype it while the
    // interceptor waits for them to try again.
    assert_eq!(EnrollmentResult::CertificateMismatch, outcome.result);
    assert!(outcome.detail.unwrap_or_default().contains("not direct"));

    assert!(fixture.accepted.lock().unwrap().iter().all(|attempt| !attempt.accepted()));
    window.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_window_accepts_nothing_at_all() {
    let fixture = Fixture::new();
    let window = fixture.open(Duration::from_secs(30)).await;
    let port = window.port();
    window.shutdown().await;

    // Not refused by a check inside the handshake -- there is nothing listening. The code is
    // unreachable rather than merely unused while the window is shut.
    assert!(fixture.join(port, CODE).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_window_closes_itself_when_its_time_is_up() {
    let fixture = Fixture::new();
    let window = fixture.open(Duration::from_millis(300)).await;

    assert!(window.is_open());
    assert!(wait_for(Duration::from_secs(15), || !window.is_open()).await);

    // And the port is released, not merely ignored.
    assert!(wait_for(Duration::from_secs(5), || std::net::TcpStream::connect(("127.0.0.1", window.port())).is_err()).await);
    window.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_of_attempts_closes_the_window_rather_than_throttling_it() {
    // Eighty bits of code is not something a person fails at eleven times in a row. Being probed is
    // the only explanation, and the answer is to stop being reachable.
    let fixture = Fixture::new();
    let window = fixture.open(Duration::from_secs(30)).await;

    for _ in 0..12 {
        if !window.is_open() {
            break;
        }

        match TcpStream::connect(("127.0.0.1", window.port())).await {
            Ok(mut probe) => {
                let _ = probe.shutdown().await;
            }
            Err(_) => break,
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(wait_for(Duration::from_secs(15), || !window.is_open()).await);
    window.shutdown().await;
}

/// A machine-in-the-middle: terminates TLS on both sides with its own certificate and copies the
/// plaintext between them without altering it. Returns the port it listens on.
async fn start_intercepting_relay(target_port: u16) -> u16 {
    let interceptor = NodeIdentity::generate("Interceptor").unwrap();
    let as_server = TlsAcceptor::from(tls::accept_any_server_config(&interceptor).unwrap());
    let as_client = TlsConnector::from(tls::accept_any_client_config(&interceptor).unwrap());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let Ok((victim, _)) = listener.accept().await else {
            return;
        };

        let Ok(upstream) = TcpStream::connect(("127.0.0.1", target_port)).await else {
            return;
        };

        let (to_victim, to_accepter) = tokio::join!(as_server.accept(victim), as_client.connect(tls::server_name(), upstream));
        let (Ok(mut to_victim), Ok(mut to_accepter)) = (to_victim, to_accepter) else {
            return;
        };

        // The victim aborting mid-exchange is the expected end of this, not a fault.
        let _ = tokio::io::copy_bidirectional(&mut to_victim, &mut to_accepter).await;
    });

    port
}
