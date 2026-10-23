//! Verifies on this machine what no build or unit test can: does a calibrated mouse delta actually
//! travel the distance it asked for, does scancode replay reach a real application, do blocking and
//! the hotkeys behave, and does the control pipe enforce its tiers against a real service.
//!
//! The mouse half is self-checking -- it injects and then measures the cursor -- and is compared
//! against uncalibrated injection so the correction is visible rather than merely asserted.
//!
//! `--uac-service` is the service's half of the secure-desktop check, driven by
//! `src/rust/spike/run-uac-check.ps1`; see [`uac`].

mod uac;

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use log::LevelFilter;
use vkms_agent::engagement::EngagementController;
use vkms_agent::health::{self, HookProbeResult};
use vkms_agent::probe;
use vkms_core::calibration::{MouseCalibration, MouseCorrectionCurve, PointerBallistics};
use vkms_core::{clock, InputEvent, InputEventFlags, InputEventKind};
use vkms_ipc::pipe_client::ControlPipeClient;
use vkms_ipc::{protocol, ControlRequest, ControlVerb, MachinesView};
use vkms_win::cursor;
use vkms_win::hook::LowLevelHook;
use vkms_win::injector::InputInjector;
use vkms_win::pointer::{self, PointerSettingsMonitor};

const RULE: &str = "==================================================================";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|arg| arg.eq_ignore_ascii_case(name));

    let value = |name: &str| {
        let index = args.iter().position(|arg| arg.eq_ignore_ascii_case(name))?;
        args.get(index + 1).cloned()
    };

    // Mode selection comes first: the verification path moves the cursor, which has no business
    // happening when something else was asked for.
    if flag("--uac-service") {
        // Run as SYSTEM by a scheduled task, with no console to log to.
        let seconds = value("--seconds").and_then(|text| text.parse().ok()).unwrap_or(40);
        std::process::exit(uac::run(Duration::from_secs(seconds)));
    }

    vkms_win::logging::init_stderr(LevelFilter::Info);

    let pipe = value("--pipe").unwrap_or_else(|| protocol::PIPE_NAME.to_owned());

    let code = if flag("--control") {
        query_service(&pipe)
    } else if flag("--engage") {
        engagement_demo()
    } else if flag("--hooks") {
        verify_hook_health()
    } else {
        verify_replay(flag("--keyboard"))
    };

    std::process::exit(code);
}

fn verify_replay(keyboard: bool) -> i32 {
    println!("Virtual KM Switch replay verification");
    println!("{RULE}");

    let ballistics = pointer::read();
    let precision = pointer::is_pointer_precision_enabled(&ballistics);

    println!(
        "this machine: threshold1={} threshold2={} acceleration={} speed={}",
        ballistics.threshold1, ballistics.threshold2, ballistics.acceleration, ballistics.speed
    );
    println!(
        "\"Enhance pointer precision\": {}",
        if precision { "ON  (chunking active)" } else { "OFF  (no acceleration curve; chunking disabled)" }
    );
    println!(
        "speed multiplier: {:.3}x, max unaccelerated chunk: {}",
        ballistics.speed_multiplier(),
        ballistics.max_unaccelerated_chunk()
    );
    println!();

    println!("Probing this machine's actual response (the cursor will move briefly)...");
    let mut injector = InputInjector::new();
    let correction = probe::measure(&mut injector, ballistics);
    println!(
        "measured correction curve across {} magnitudes: 10px={:.3}x  50px={:.3}x  200px={:.3}x",
        correction.sample_count(),
        correction.factor_for(10),
        correction.factor_for(50),
        correction.factor_for(200)
    );
    println!();

    let mut failures = verify_mouse_travel(ballistics, correction);

    if keyboard {
        verify_keyboard_replay();
    } else {
        println!();
        println!("Skipping the keyboard check (needs a focused Notepad). Run with --keyboard to include it.");
    }

    failures += verify_hook_health();

    println!();
    println!("Watching pointer settings for 6s -- change the speed slider or toggle");
    println!("\"Enhance pointer precision\" now to see it detected.");

    {
        let _monitor = PointerSettingsMonitor::new(Duration::from_secs(1), |changed| {
            println!(
                "  CHANGE DETECTED: speed={} acceleration={} (pointer precision {}) -- calibration is now stale and \
                 would be re-probed",
                changed.speed,
                changed.acceleration,
                if pointer::is_pointer_precision_enabled(&changed) { "ON" } else { "OFF" }
            );
        });

        sleep(Duration::from_secs(6));
    }

    println!();
    println!("{RULE}");
    if failures == 0 {
        println!("RESULT: replay verified.");
    } else {
        println!("RESULT: {failures} check(s) failed.");
    }

    failures
}

fn verify_mouse_travel(ballistics: PointerBallistics, correction: MouseCorrectionCurve) -> i32 {
    let mut injector = InputInjector::new();
    let mut calibration = MouseCalibration::new(ballistics).with_correction(correction);
    let origin = cursor::position().unwrap_or((0, 0));

    println!("Injecting horizontal moves and measuring actual cursor travel.");
    println!();
    println!("  requested | uncalibrated | calibrated | calibrated error");
    println!("  {}", "-".repeat(60));

    let mut failures = 0;

    for requested in [10, 25, 50, 100, 200, 400] {
        let start = begin_measure();
        injector.inject(&[InputEvent::mouse_move(clock::ticks(), requested, 0)]);
        let uncalibrated = end_measure(start);

        calibration.reset();
        let mut chunks = [InputEvent::default(); MouseCalibration::MAX_CHUNKS_PER_EVENT];
        let start = begin_measure();
        let count = calibration.prepare(clock::ticks(), requested, 0, &mut chunks);
        injector.inject(&chunks[..count]);
        let calibrated = end_measure(start);

        let error = calibrated - requested;

        // A little slack: the measurement races the input pipeline, and the cursor clamps at edges.
        let ok = error.abs() <= (requested / 20).max(2);
        if !ok {
            failures += 1;
        }

        println!(
            "  {requested:>9} | {uncalibrated:>12} | {calibrated:>10} | {error:>4} {}",
            if ok { "ok" } else { "FAIL" }
        );
    }

    cursor::set_position(origin.0, origin.1);
    failures
}

/// Parks the cursor well away from any screen edge, which would clamp travel.
fn begin_measure() -> i32 {
    cursor::set_position(400, 400);
    sleep(Duration::from_millis(60));
    cursor::position().map_or(0, |(x, _)| x)
}

fn end_measure(start: i32) -> i32 {
    sleep(Duration::from_millis(120)); // let the input pipeline settle
    cursor::position().map_or(0, |(x, _)| x) - start
}

fn verify_keyboard_replay() {
    println!();
    println!("Keyboard replay: focus a Notepad window within 5 seconds...");
    for remaining in (1..=5).rev() {
        print!("{remaining}... ");
        sleep(Duration::from_secs(1));
    }
    println!();

    let mut injector = InputInjector::new();

    // Scancodes for "virtualkm ok" on a US layout. Scancodes rather than virtual keys is the whole
    // point: the target reproduces the physical key, not a layout-dependent character.
    for scan in [0x2F, 0x17, 0x13, 0x14, 0x16, 0x1E, 0x26, 0x25, 0x32, 0x39, 0x18, 0x25] {
        let now = clock::ticks();
        injector.inject(&[InputEvent::key(now, scan, InputEventFlags::NONE), InputEvent::key(now, scan, InputEventFlags::UP)]);
        sleep(Duration::from_millis(30));
    }

    println!("  Injected. Check whether the text appeared in Notepad.");
}

/// Confirms the watchdog can tell a live hook from a dead one, and that reinstalling recovers it. A
/// liveness probe that always answers "alive" is worse than no watchdog at all, so both directions
/// are checked.
fn verify_hook_health() -> i32 {
    println!();
    println!("Hook liveness watchdog:");

    let mut injector = InputInjector::new();
    let hook = match LowLevelHook::new(None) {
        Ok(hook) => hook,
        Err(error) => {
            println!("  could not install hooks: {error}");
            return 1;
        }
    };

    let alive = health::probe(&hook, &mut injector);
    println!("  hook installed            : {alive:?}");

    // Exactly what Windows does on a timeout: the hooks go, and nothing says so.
    hook.unhook_silently();
    sleep(Duration::from_millis(50));
    let dead = health::probe(&hook, &mut injector);
    println!("  hook silently removed     : {dead:?}  (the object still says installed={})", hook.is_installed());

    hook.reinstall();
    sleep(Duration::from_millis(50));
    let recovered = health::probe(&hook, &mut injector);
    println!("  after reinstall           : {recovered:?}");

    let ok = alive == HookProbeResult::Alive && dead == HookProbeResult::Dead && recovered == HookProbeResult::Alive;
    println!("  => watchdog {}", if ok { "detects a silent unhook and recovers" } else { "IS NOT RELIABLE" });
    i32::from(!ok)
}

/// Drives blocking, capture and the hotkeys by hand. Nothing is sent anywhere -- captured events are
/// printed -- so this shows exactly what would have gone to the service, and what this machine was
/// prevented from seeing. The service's part (turning a hotkey into "engaged") is played here.
fn engagement_demo() -> i32 {
    println!("Engagement demo -- 30 seconds.");
    println!();
    println!("  Ctrl+Alt+1          engage (keyboard and mouse go dead locally)");
    println!("  Ctrl+Alt+0          return control");
    println!("  Ctrl+Alt+Shift+Esc  panic");
    println!();
    println!("While engaged, typing and mouse buttons are captured and shown here instead of reaching the desktop.");
    println!();

    // A hard backstop: blocking must never outlive this process, whatever else goes wrong.
    std::thread::spawn(|| {
        sleep(Duration::from_secs(40));
        std::process::exit(99);
    });

    let captured = Arc::new(AtomicI64::new(0));
    let engaged = Arc::new(AtomicBool::new(false));
    let (decision_tx, decision_rx) = std::sync::mpsc::channel::<bool>();

    let counter = Arc::clone(&captured);
    let sink = Arc::new(move |event: &InputEvent| {
        if let Some((action, slot)) = event.hotkey_action() {
            println!("  HOTKEY: {action:?}{}", if slot > 0 { format!(" slot {slot}") } else { String::new() });

            // What the service would decide. Handed to another thread: this runs inside the hook,
            // where injecting the local modifier releases would re-enter it.
            let _ = decision_tx.send(action == vkms_core::hotkeys::HotkeyActionKind::Engage);
            return;
        }

        let total = counter.fetch_add(1, Ordering::Relaxed) + 1;

        // Keys and buttons only: motion at kilohertz rates would bury them.
        if event.kind != InputEventKind::MOUSE_MOVE && total < 200 {
            println!("    -> {:?} code={:#x} flags={:#x}", event.kind, event.code, event.flags.0);
        }
    });

    let controller = match EngagementController::new(sink, Arc::clone(&engaged), || true) {
        Ok(controller) => controller,
        Err(error) => {
            println!("could not install capture: {error}");
            return 1;
        }
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        if let Ok(engage) = decision_rx.recv_timeout(remaining) {
            controller.set_engaged(engage);
            println!("  STATE: {}", if engage { "engaged" } else { "local" });
        }
    }

    println!();
    println!("  captured {} events while engaged", captured.load(Ordering::Relaxed));
    println!("  final state: {}", if controller.is_engaged() { "engaged" } else { "local" });
    0
}

/// Talks to the running service over the control pipe, as whoever runs this.
///
/// The cross-privilege check: a LocalSystem service and an ordinary client, with the two-tier
/// authorisation deciding what the caller may do from its own token rather than from anything it
/// claims.
fn query_service(pipe: &str) -> i32 {
    println!("Control pipe check");
    println!("{RULE}");

    let mut client = match ControlPipeClient::connect(pipe, Duration::from_secs(5)) {
        Ok(client) => client,
        Err(error) => {
            println!("could not connect: {error}");
            println!("Is the service installed and running?");
            return 1;
        }
    };

    println!("connected to the service");
    println!();
    println!("-- runtime-tier verb (should succeed) --");

    let status = match client.send(&ControlRequest::new(ControlVerb::GetStatus)) {
        Ok(status) => status,
        Err(error) => {
            println!("  request failed: {error}");
            return 1;
        }
    };

    println!("  success: {}", status.success);
    let mut values: Vec<_> = status.values.clone().unwrap_or_default().into_iter().collect();
    values.sort();
    for (key, value) in values {
        println!("    {key} = {value}");
    }

    println!();
    println!("-- paired machines (runtime tier: reading the list is not privileged) --");
    match client.send(&ControlRequest::new(ControlVerb::GetMachines)).ok().and_then(|response| MachinesView::from_response(&response)) {
        Some(view) => {
            println!("  this machine: {}", view.local_node_id);
            println!("  health      : {:?}", view.health);
            println!("  pairing     : {}", if view.pairing_open { format!("open, {}s left", view.pairing_seconds_left) } else { "closed".into() });

            for machine in view.machines {
                println!(
                    "    {}{}  {}",
                    machine.label,
                    if machine.is_local { " (this machine)" } else if machine.is_connected { " (connected)" } else { "" },
                    machine.node_id.simple()
                );
            }
        }
        None => println!("  no list returned"),
    }

    println!();
    println!("-- administrative verb (should be refused unless elevated) --");
    // The nil id is never a paired machine, so run elevated this removes nothing.
    match client.send(&ControlRequest::new(ControlVerb::RemovePeer).with("nodeId", "00000000-0000-0000-0000-000000000000")) {
        Ok(response) => {
            println!("  success: {}", response.success);
            println!("  error  : {}", response.error.unwrap_or_default());
        }
        Err(error) => println!("  request failed: {error}"),
    }

    println!();
    println!("{RULE}");
    i32::from(!status.success)
}
