//! Two complete services in one process, paired by code and driving each other over real TLS.
//!
//! This composes exactly what the real service composes -- registry, router, mesh host, control
//! endpoint -- rather than mocking the joins between them, because the joins are where wiring goes
//! wrong while every component test still passes. Only the agents are replaced: each machine gets a
//! ring of its own in the Local namespace, read here to see what would have been injected.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;
use vkms_core::hotkeys::HotkeyActionKind;
use vkms_core::{clock, InputEvent, InputEventFlags, InputEventKind};
use vkms_ipc::pipe_client::ControlPipeClient;
use vkms_ipc::{AgentChannel, ControlRequest, ControlResponse, ControlTier, ControlVerb, MachinesView};
use vkms_service::mesh_host::MeshOptions;
use vkms_service::service::{self, RunningService, ServiceOptions};

struct Machine {
    name: String,
    dir: PathBuf,
    pipe: String,
    running: Option<RunningService>,

    /// Stands in for the session agent: what the service would have handed it to inject.
    agent: Arc<AgentChannel>,
}

impl Machine {
    async fn start(name: &str) -> Self {
        let tag = Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join(format!("vkms-service-{name}-{tag}"));
        let pipe = format!("VirtualKmSwitch.Test.{name}.{tag}");

        let options = ServiceOptions {
            data_dir: dir.clone(),
            pipe_name: pipe.clone(),
            agents: false,
            agent_directory: dir.clone(),
            display_name: name.to_owned(),
            mesh: MeshOptions {
                mesh_port: 0,
                beacon_port: 0,
                pairing_port: 0,
                pairing_duration: Duration::from_secs(30),
                discovery: false,
            },
        };

        let running = service::start(options).await.expect("the service starts");

        let agent = Arc::new(
            AgentChannel::create(&format!("Test{name}{}", &tag[..8]), std::process::id(), 1024, false, false)
                .expect("a local agent channel"),
        );
        running.channels.set(vec![Arc::clone(&agent)]);

        Self { name: name.to_owned(), dir, pipe, running: Some(running), agent }
    }

    fn service(&self) -> &RunningService {
        self.running.as_ref().unwrap()
    }

    fn id(&self) -> Uuid {
        self.service().registry.local_node_id()
    }

    /// As an administrator would ask; the pipe's own tier check is covered separately.
    async fn ask(&self, request: ControlRequest) -> ControlResponse {
        self.service().endpoint.handle(request, ControlTier::Administrative).await
    }

    /// Everything handed to this machine's agent so far.
    fn injected(&self) -> Vec<InputEvent> {
        let mut all = Vec::new();
        let mut batch = [InputEvent::default(); 256];
        loop {
            let count = self.agent.to_agent.read_batch(&mut batch);
            if count == 0 {
                return all;
            }
            all.extend_from_slice(&batch[..count]);
        }
    }

    async fn stop(mut self) {
        if let Some(running) = self.running.take() {
            running.stop().await;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    panic!("timed out waiting for {what}");
}

/// Collects what reaches `machine`'s agent until `done` is satisfied.
async fn collect_until(machine: &Machine, what: &str, mut done: impl FnMut(&[InputEvent]) -> bool) -> Vec<InputEvent> {
    let mut seen = Vec::new();
    wait_for(what, || {
        seen.extend(machine.injected());
        done(&seen)
    })
    .await;
    seen
}

/// Pairs `typer` with `shower` the way a person would: one shows a code, the other types it in.
async fn pair(shower: &Machine, typer: &Machine) -> ControlResponse {
    let opened = shower.ask(ControlRequest::new(ControlVerb::OpenPairingWindow)).await;
    assert!(opened.success, "{:?}", opened.error);

    let code = opened.value("code").expect("the code is returned once, to whoever opened the window").to_owned();
    let port = shower.service().mesh.pairing_port().expect("a window is open");

    typer
        .ask(ControlRequest::new(ControlVerb::PairWithCode).with("code", code).with("host", format!("127.0.0.1:{port}")))
        .await
}

async fn paired_and_linked() -> (Machine, Machine) {
    let (alice, bob) = (Machine::start("Alice").await, Machine::start("Bob").await);

    let result = pair(&alice, &bob).await;
    assert!(result.success, "{:?}", result.error);

    let (alice_mesh, bob_mesh) = (Arc::clone(&alice.service().mesh), Arc::clone(&bob.service().mesh));
    let (alice_id, bob_id) = (alice.id(), bob.id());
    wait_for("the link to come up on both sides", || {
        alice_mesh.connected_peers().contains(&bob_id) && bob_mesh.connected_peers().contains(&alice_id)
    })
    .await;

    (alice, bob)
}

fn key(code: u16, flags: InputEventFlags) -> InputEvent {
    InputEvent::key(clock::ticks(), code, flags)
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_by_code_trusts_both_ways_and_brings_the_link_up() {
    let (alice, bob) = paired_and_linked().await;

    // Each lists the other, under the name it gave, and nothing else is shared.
    let view = MachinesView::from_response(&alice.ask(ControlRequest::new(ControlVerb::GetMachines)).await).unwrap();
    let bob_entry = view.machines.iter().find(|machine| machine.node_id == bob.id()).expect("Bob is listed");
    assert_eq!("Bob", bob_entry.label);
    assert!(bob_entry.is_connected);

    assert!(bob.service().registry.is_paired(alice.id()));

    // The window closed on success: a code that has done its job stops working.
    assert!(alice.service().mesh.pairing_left().is_none());

    // And the newly paired machine works without configuration: it took the first free hotkey.
    assert_eq!(Some(bob.id()), alice.service().registry.settings().load().target_for(1));
    assert_eq!(Some(alice.id()), bob.service().registry.settings().load().target_for(1));

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_code_is_refused_and_nothing_is_trusted() {
    let (alice, bob) = (Machine::start("Alice").await, Machine::start("Bob").await);

    let opened = alice.ask(ControlRequest::new(ControlVerb::OpenPairingWindow)).await;
    assert!(opened.success);
    let port = alice.service().mesh.pairing_port().unwrap();

    let result = bob
        .ask(
            ControlRequest::new(ControlVerb::PairWithCode)
                .with("code", "ZZZZ-ZZZZ-ZZZZ-ZZZZ")
                .with("host", format!("127.0.0.1:{port}")),
        )
        .await;

    assert!(!result.success);
    assert!(result.error.unwrap_or_default().contains("not accepted"));
    assert!(alice.service().registry.peers().is_empty());
    assert!(bob.service().registry.peers().is_empty());

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_code_is_refused_before_anything_is_sent() {
    let bob = Machine::start("Bob").await;

    let result = bob.ask(ControlRequest::new(ControlVerb::PairWithCode).with("code", "ABCD")).await;

    assert!(!result.success);
    assert!(result.error.unwrap_or_default().contains("16 characters"));
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_crosses_the_link_until_the_machine_is_engaged() {
    // Paired and connected must not mean driven, or every machine would type into every other.
    let (alice, bob) = paired_and_linked().await;

    alice.service().router.capture(&key(30, InputEventFlags::NONE));
    tokio::time::sleep(Duration::from_millis(250)).await;

    assert!(bob.injected().is_empty());
    assert!(!alice.agent.engaged(), "nothing engaged, so the agent must not be blocking local input");

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keystroke_captured_while_engaged_is_injected_on_the_machine_being_driven() {
    let (alice, bob) = paired_and_linked().await;

    let engaged = alice.ask(ControlRequest::new(ControlVerb::Engage).with("nodeId", bob.id().to_string())).await;
    assert!(engaged.success, "{:?}", engaged.error);

    // The agent is told to block local input: this machine's keyboard now belongs to Bob.
    assert!(alice.agent.engaged());

    alice.service().router.capture(&key(30, InputEventFlags::NONE));
    alice.service().router.capture(&key(30, InputEventFlags::UP));

    let injected = collect_until(&bob, "both key transitions", |seen| seen.len() >= 2).await;
    assert_eq!((InputEventKind::KEY, 30, false), (injected[0].kind, injected[0].code, injected[0].is_up()));

    // The release matters as much as the press: it is the half that, when lost, leaves a key held
    // down on a machine the user cannot reach from where they are sitting.
    assert!(injected[1].is_up());

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn returning_control_releases_what_was_held_on_the_machine_left() {
    let (alice, bob) = paired_and_linked().await;

    alice.service().router.engage(bob.id());
    alice.service().router.capture(&key(42, InputEventFlags::NONE));
    alice.service().router.capture(&InputEvent::mouse_button(clock::ticks(), 0, false));

    collect_until(&bob, "the press to arrive", |seen| seen.len() >= 2).await;

    alice.ask(ControlRequest::new(ControlVerb::Disengage)).await;
    assert!(!alice.agent.engaged());

    // Released on the receiving side, from what it was actually told to press -- and the marker
    // itself is not handed to the agent, which would not know what to do with it.
    let released = collect_until(&bob, "both releases", |seen| seen.iter().filter(|e| e.is_up()).count() >= 2).await;
    assert!(released.iter().any(|e| e.kind == InputEventKind::KEY && e.code == 42 && e.is_up()));
    assert!(released.iter().any(|e| e.kind == InputEventKind::MOUSE_BUTTON && e.is_up()));
    assert!(released.iter().all(|e| e.kind != InputEventKind::RELEASE_ALL));

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hotkey_matched_by_the_agent_drives_the_machine_bound_to_that_slot() {
    let (alice, bob) = paired_and_linked().await;

    // What an agent sends when Ctrl+Alt+1 is pressed. Slot 1 is Bob, by default, on Alice's own map.
    alice.service().router.capture(&InputEvent::hotkey(clock::ticks(), HotkeyActionKind::Engage, 1));
    assert_eq!(Some(bob.id()), alice.service().router.target());
    assert!(alice.agent.engaged());

    alice.service().router.capture(&InputEvent::hotkey(clock::ticks(), HotkeyActionKind::ReturnLocal, 0));
    assert_eq!(None, alice.service().router.target());

    // An unassigned slot does nothing at all, rather than something surprising.
    alice.service().router.capture(&InputEvent::hotkey(clock::ticks(), HotkeyActionKind::Engage, 7));
    assert_eq!(None, alice.service().router.target());

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mouse_motion_merges_on_the_way_out_but_the_click_stays_behind_it() {
    let (alice, bob) = paired_and_linked().await;
    alice.service().router.engage(bob.id());

    for _ in 0..50 {
        alice.service().router.capture(&InputEvent::mouse_move(clock::ticks(), 2, 1));
    }
    alice.service().router.capture(&InputEvent::mouse_button(clock::ticks(), 0, false));

    let received =
        collect_until(&bob, "the click", |seen| seen.iter().any(|e| e.kind == InputEventKind::MOUSE_BUTTON)).await;

    // Every pixel of movement is still there; it simply travelled in fewer events.
    let motion: Vec<_> = received.iter().filter(|e| e.kind == InputEventKind::MOUSE_MOVE).collect();
    assert_eq!(100, motion.iter().map(|e| e.x).sum::<i32>());
    assert_eq!(50, motion.iter().map(|e| e.y).sum::<i32>());
    assert_eq!(InputEventKind::MOUSE_BUTTON, received.last().unwrap().kind);

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn input_flows_both_ways_at_once_because_either_machine_may_be_driving() {
    let (alice, bob) = paired_and_linked().await;

    alice.service().router.engage(bob.id());
    bob.service().router.engage(alice.id());

    alice.service().router.capture(&key(30, InputEventFlags::NONE));
    bob.service().router.capture(&key(31, InputEventFlags::NONE));

    collect_until(&bob, "Alice's keystroke to reach Bob", |seen| seen.iter().any(|e| e.code == 30)).await;
    collect_until(&alice, "Bob's keystroke to reach Alice", |seen| seen.iter().any(|e| e.code == 31)).await;

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn engaging_explains_why_it_cannot() {
    let alice = Machine::start("Alice").await;

    let myself = alice.ask(ControlRequest::new(ControlVerb::Engage).with("nodeId", alice.id().to_string())).await;
    assert!(myself.error.unwrap_or_default().contains("already the one you are typing on"));

    let stranger = alice.ask(ControlRequest::new(ControlVerb::Engage).with("nodeId", Uuid::new_v4().to_string())).await;
    assert!(stranger.error.unwrap_or_default().contains("not paired"));

    let missing = alice.ask(ControlRequest::new(ControlVerb::Engage)).await;
    assert!(missing.error.unwrap_or_default().contains("nodeId"));

    alice.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unpairing_drops_the_link_the_trust_and_the_hotkey() {
    let (alice, bob) = paired_and_linked().await;
    alice.service().router.engage(bob.id());

    let removed = alice.ask(ControlRequest::new(ControlVerb::RemovePeer).with("nodeId", bob.id().to_string())).await;
    assert!(removed.success, "{:?}", removed.error);

    // Control came back first, so the release could still reach Bob.
    assert_eq!(None, alice.service().router.target());
    assert!(!alice.service().registry.pins().is_trusted(bob.service().registry.identity().thumbprint()));
    assert_eq!(None, alice.service().registry.settings().load().target_for(1));

    let (alice_mesh, bob_mesh) = (Arc::clone(&alice.service().mesh), Arc::clone(&bob.service().mesh));
    let (alice_id, bob_id) = (alice.id(), bob.id());
    wait_for("both ends to drop the link", || {
        !alice_mesh.connected_peers().contains(&bob_id) && !bob_mesh.connected_peers().contains(&alice_id)
    })
    .await;

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hotkeys_can_only_point_at_paired_machines() {
    let (alice, bob) = paired_and_linked().await;

    let stranger = alice
        .ask(ControlRequest::new(ControlVerb::SetHotkeys).with("slot", "4").with("target", Uuid::new_v4().to_string()))
        .await;
    assert!(!stranger.success);

    let moved = alice
        .ask(ControlRequest::new(ControlVerb::SetHotkeys).with("slot", "4").with("target", bob.id().to_string()))
        .await;
    assert!(moved.success);

    let hotkeys = alice.ask(ControlRequest::new(ControlVerb::GetHotkeys)).await;
    assert_eq!(Some(bob.id().to_string().as_str()), hotkeys.value("4"));
    assert_eq!(Some(""), hotkeys.value("1"), "moving a machine to another key empties the old one");

    let zero = alice.ask(ControlRequest::new(ControlVerb::SetHotkeys).with("slot", "0").with("target", "")).await;
    assert!(zero.error.unwrap_or_default().contains("Slot 0"));

    alice.stop().await;
    bob.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_controller_reaches_the_service_over_the_real_pipe() {
    let alice = Machine::start("Alice").await;
    let pipe = alice.pipe.clone();

    let (status, refused) = tokio::task::spawn_blocking(move || {
        let mut client = ControlPipeClient::connect(&pipe, Duration::from_secs(5)).unwrap();
        let status = client.send(&ControlRequest::new(ControlVerb::GetStatus)).unwrap();
        let refused = client.send(&ControlRequest::new(ControlVerb::OpenPairingWindow)).unwrap();
        (status, refused)
    })
    .await
    .unwrap();

    assert!(status.success);
    assert_eq!(Some(alice.id().to_string().as_str()), status.value("localNodeId"));
    assert_eq!(Some(alice.name.as_str()), status.value("machineName"));

    // Opening a pairing window hands out a way in, so it is administrative. An elevated test run is
    // allowed through; an ordinary one must be refused.
    if !refused.success {
        assert!(refused.error.unwrap_or_default().contains("administrator"));
    }

    alice.stop().await;
}
