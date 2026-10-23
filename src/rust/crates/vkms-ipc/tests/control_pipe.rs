//! End to end over a real named pipe: a real server, a real client, real ACLs. Only the caller's
//! privilege is uncontrolled, since a test process cannot drop to a different token on demand.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vkms_ipc::pipe_client::ControlPipeClient;
use vkms_ipc::pipe_server::{BoxFuture, ControlHandler, ControlPipeServer};
use vkms_ipc::{ControlRequest, ControlResponse, ControlTier, ControlVerb};

/// A unique pipe per test. The production name would attach to a Virtual KM Switch service if one happens
/// to be installed, and the tests would silently exercise that instead.
fn pipe_name(test: &str) -> String {
    format!("VirtualKmSwitch.Test.{test}.{}", std::process::id())
}

fn handler(
    respond: impl Fn(ControlRequest, ControlTier) -> ControlResponse + Send + Sync + 'static,
) -> ControlHandler {
    let respond = Arc::new(respond);
    Arc::new(move |request, tier| {
        let respond = Arc::clone(&respond);
        Box::pin(async move { respond(request, tier) }) as BoxFuture<ControlResponse>
    })
}

async fn send_all(name: String, requests: Vec<ControlRequest>) -> Vec<ControlResponse> {
    tokio::task::spawn_blocking(move || {
        let mut client = ControlPipeClient::connect(&name, Duration::from_secs(5)).expect("connect");
        requests.iter().map(|request| client.send(request).expect("send")).collect()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_round_trip_over_the_real_pipe() {
    let name = pipe_name("round_trip");
    let server = ControlPipeServer::start(
        &name,
        handler(|request, _| {
            ControlResponse::ok_with(HashMap::from([("echo".to_owned(), format!("{:?}", request.verb))]))
        }),
    )
    .unwrap();

    let responses = send_all(name, vec![ControlRequest::new(ControlVerb::GetStatus)]).await;

    assert!(responses[0].success);
    assert_eq!(Some("GetStatus"), responses[0].value("echo"));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn arguments_survive_the_round_trip() {
    let seen = Arc::new(Mutex::new(None));
    let record = Arc::clone(&seen);

    let name = pipe_name("arguments");
    let server = ControlPipeServer::start(
        &name,
        handler(move |request, _| {
            *record.lock().unwrap() = request.argument("slot").map(str::to_owned);
            ControlResponse::ok()
        }),
    )
    .unwrap();

    send_all(name, vec![ControlRequest::new(ControlVerb::Engage).with("slot", "3")]).await;

    assert_eq!(Some("3".to_owned()), *seen.lock().unwrap());
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn several_requests_share_one_connection() {
    let name = pipe_name("several");
    let server = ControlPipeServer::start(
        &name,
        handler(|request, _| {
            ControlResponse::ok_with(HashMap::from([("verb".to_owned(), format!("{:?}", request.verb))]))
        }),
    )
    .unwrap();

    // Responses are matched to requests by order alone, so this is what proves the framing stays
    // aligned across messages.
    let verbs = [ControlVerb::GetStatus, ControlVerb::Engage, ControlVerb::Disengage];
    let responses = send_all(name, verbs.iter().map(|verb| ControlRequest::new(*verb)).collect()).await;

    for (verb, response) in verbs.iter().zip(&responses) {
        assert_eq!(Some(format!("{verb:?}").as_str()), response.value("verb"));
    }

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handler_that_panics_becomes_a_failure_not_a_dropped_connection() {
    let name = pipe_name("panics");
    let server = ControlPipeServer::start(&name, handler(|_, _| panic!("handler exploded"))).unwrap();

    let responses = send_all(
        name,
        vec![ControlRequest::new(ControlVerb::GetStatus), ControlRequest::new(ControlVerb::GetStatus)],
    )
    .await;

    assert!(!responses[0].success);
    assert!(responses[0].error.as_deref().unwrap_or_default().contains("handler exploded"));

    // And the connection is still good for the next request.
    assert!(!responses[1].success);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_handler_is_told_which_tier_the_caller_has() {
    let observed = Arc::new(Mutex::new(None));
    let record = Arc::clone(&observed);

    let name = pipe_name("tier");
    let server = ControlPipeServer::start(
        &name,
        handler(move |_, tier| {
            *record.lock().unwrap() = Some(tier);
            ControlResponse::ok()
        }),
    )
    .unwrap();

    send_all(name, vec![ControlRequest::new(ControlVerb::GetStatus)]).await;

    // The value depends on whether the test process is elevated; what matters is that the server
    // determined it from the connection rather than from anything the client sent.
    assert!(observed.lock().unwrap().is_some());
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn administrative_verbs_are_refused_to_a_runtime_caller_before_the_handler_runs() {
    if vkms_win::security::is_system() {
        return; // nothing to refuse to SYSTEM
    }

    let reached = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&reached);

    let name = pipe_name("refused");
    let server = ControlPipeServer::start(
        &name,
        handler(move |request, tier| {
            record.lock().unwrap().push((request.verb, tier));
            ControlResponse::ok()
        }),
    )
    .unwrap();

    let responses = send_all(name, vec![ControlRequest::new(ControlVerb::RemovePeer)]).await;

    let first = reached.lock().unwrap().first().copied();
    match first {
        // An elevated test run is an administrator, so the request is allowed through.
        Some((_, ControlTier::Administrative)) => assert!(responses[0].success),
        Some(other) => panic!("a runtime caller reached the handler with {other:?}"),
        None => {
            assert!(!responses[0].success);
            assert!(responses[0].error.as_deref().unwrap_or_default().contains("administrator"));
        }
    }

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_server_cannot_take_over_a_pipe_name_already_in_use() {
    // FIRST_PIPE_INSTANCE: a squatter that created the name first makes the service fail at
    // startup, rather than leave the controller talking to whichever end answered.
    let name = pipe_name("squat");
    let first = ControlPipeServer::start(&name, handler(|_, _| ControlResponse::ok())).unwrap();

    assert!(ControlPipeServer::start(&name, handler(|_, _| ControlResponse::ok())).is_err());
    first.shutdown().await;
}
