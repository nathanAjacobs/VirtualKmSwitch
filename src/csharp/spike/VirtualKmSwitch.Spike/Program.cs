using System.Diagnostics;
using VirtualKmSwitch.Core;
using VirtualKmSwitch.InputEngine;
using VirtualKmSwitch.Interop;

namespace VirtualKmSwitch.Spike;

/// <summary>
/// Verifies replay end to end on this machine: does a calibrated mouse delta actually travel the
/// distance it asked for, and does scancode keyboard replay reach a real application?
///
/// The mouse half is self-checking -- it injects and then measures the cursor -- because the whole
/// point of calibration is that the number requested and the number travelled agree, and that is
/// measurable without a human. It is compared against uncalibrated injection so the correction is
/// visible rather than merely asserted.
/// </summary>
internal static class Program
{
    private static int Main(string[] args)
    {
        // Mode selection comes first: the verification path probes by moving the cursor, which
        // has no business running when the caller asked for something else.
        if (args.Contains("--control", StringComparer.OrdinalIgnoreCase))
        {
            return QueryServiceAsync().GetAwaiter().GetResult();
        }

        if (args.Contains("--engage", StringComparer.OrdinalIgnoreCase))
        {
            return RunEngagementDemo();
        }

        if (args.Contains("--tray", StringComparer.OrdinalIgnoreCase))
        {
            return RunTrayCheck();
        }

        Console.WriteLine("Virtual KM Switch replay verification");
        Console.WriteLine(new string('=', 66));

        (int t1, int t2, int accel, int speed) = PointerSettings.Read();
        var ballistics = new PointerBallistics(t1, t2, accel, speed);

        Console.WriteLine($"this machine: threshold1={t1} threshold2={t2} acceleration={accel} speed={speed}");
        Console.WriteLine($"\"Enhance pointer precision\": {(accel != 0 ? "ON" : "OFF")}"
            + (accel != 0 ? "  (chunking active)" : "  (no acceleration curve; chunking disabled)"));
        Console.WriteLine($"speed multiplier: {ballistics.SpeedMultiplier:0.###}x, "
            + $"max unaccelerated chunk: {ballistics.MaxUnacceleratedChunk}");
        Console.WriteLine();

        Console.WriteLine("Probing this machine's actual response (the cursor will move briefly)...");
        var probeInjector = new InputInjector();
        MouseCorrectionCurve correction = MouseCalibrationProbe.Measure(probeInjector, ballistics);
        Console.WriteLine($"measured correction curve across {correction.SampleCount} magnitudes: "
            + $"10px={correction.FactorFor(10):0.###}x  50px={correction.FactorFor(50):0.###}x  "
            + $"200px={correction.FactorFor(200):0.###}x");
        Console.WriteLine();

        int failures = 0;
        failures += VerifyMouseTravel(ballistics, correction);

        if (args.Contains("--keyboard", StringComparer.OrdinalIgnoreCase))
        {
            VerifyKeyboardReplay();
        }
        else
        {
            Console.WriteLine();
            Console.WriteLine("Skipping the keyboard check (needs a focused Notepad).");
            Console.WriteLine("Run with --keyboard to include it.");
        }

        failures += VerifyHookHealth();

        Console.WriteLine();
        Console.WriteLine("Watching pointer settings for 6s -- change the speed slider or toggle");
        Console.WriteLine("\"Enhance pointer precision\" now to see it detected.");

        using (var monitor = new PointerSettingsMonitor(
            changed => Console.WriteLine($"  CHANGE DETECTED: speed={changed.Speed} "
                + $"acceleration={changed.Acceleration} "
                + $"(pointer precision {(changed.IsPointerPrecisionEnabled ? "ON" : "OFF")}) "
                + "-- calibration is now stale and would be re-probed"),
            TimeSpan.FromSeconds(1)))
        {
            Thread.Sleep(TimeSpan.FromSeconds(6));
        }

        Console.WriteLine();
        Console.WriteLine(new string('=', 66));
        Console.WriteLine(failures == 0 ? "RESULT: replay verified." : $"RESULT: {failures} check(s) failed.");
        return failures;
    }

    /// <summary>
    /// Talks to the running service over the control pipe, as an ordinary unelevated user.
    ///
    /// This is the cross-privilege check: a LocalSystem service and a normal-user client, with the
    /// two-tier authorisation deciding what the caller may do from its own token rather than from
    /// anything it claims.
    /// </summary>
    private static async Task<int> QueryServiceAsync()
    {
        Console.WriteLine("Control pipe check (running unelevated)");
        Console.WriteLine(new string('=', 66));

        await using var client = new VirtualKmSwitch.Ipc.ControlPipeClient();

        try
        {
            await client.ConnectAsync(TimeSpan.FromSeconds(5));
            Console.WriteLine("connected to the service");
        }
        catch (Exception ex)
        {
            Console.WriteLine($"could not connect: {ex.Message}");
            Console.WriteLine("Is the service installed and running?");
            return 1;
        }

        Console.WriteLine();
        Console.WriteLine("-- runtime-tier verb (should succeed) --");
        VirtualKmSwitch.Ipc.ControlResponse status = await client.SendAsync(
            new VirtualKmSwitch.Ipc.ControlRequest { Verb = VirtualKmSwitch.Ipc.ControlVerb.GetStatus });

        Console.WriteLine($"  success: {status.Success}");
        if (status.Values is not null)
        {
            foreach ((string key, string value) in status.Values)
            {
                Console.WriteLine($"    {key} = {value}");
            }
        }

        Console.WriteLine();
        Console.WriteLine("-- membership (runtime tier: reading who is a member is not privileged) --");
        VirtualKmSwitch.Ipc.ControlResponse machines = await client.SendAsync(
            new VirtualKmSwitch.Ipc.ControlRequest { Verb = VirtualKmSwitch.Ipc.ControlVerb.GetMachines });

        VirtualKmSwitch.Ipc.ClusterView? view = VirtualKmSwitch.Ipc.ClusterView.FromResponse(machines);
        if (view is null)
        {
            Console.WriteLine($"  no view returned: {machines.Error}");
        }
        else
        {
            Console.WriteLine($"  cluster    : {(view.HasCluster ? $"'{view.ClusterName}' ({view.ClusterId})" : "none")}");
            Console.WriteLine($"  this machine: {view.LocalNodeId}");
            Console.WriteLine($"  health     : {view.Health}");
            Console.WriteLine($"  join key   : {(view.JoinKey is null ? "not disclosed" : "DISCLOSED -- it should not be")}");

            foreach (VirtualKmSwitch.Ipc.MachineInfo machine in view.Machines)
            {
                Console.WriteLine($"    {machine.Label}{(machine.IsLocal ? " (this machine)" : string.Empty)}"
                    + $"  {machine.NodeId:N}");
            }
        }

        Console.WriteLine();
        Console.WriteLine("-- administrative verb (should be refused unless elevated) --");
        VirtualKmSwitch.Ipc.ControlResponse enroll = await client.SendAsync(
            new VirtualKmSwitch.Ipc.ControlRequest { Verb = VirtualKmSwitch.Ipc.ControlVerb.EnrollPeer });

        Console.WriteLine($"  success: {enroll.Success}");
        Console.WriteLine($"  error  : {enroll.Error}");

        Console.WriteLine();
        Console.WriteLine(new string('=', 66));
        return status.Success ? 0 : 1;
    }

    /// <summary>
    /// Puts a real icon in the notification area and reports what the user does with it.
    ///
    /// Shell_NotifyIcon failing is the one thing here that cannot be inferred from a build, and
    /// the controller deliberately swallows that failure rather than refusing to start — so this
    /// is where it is allowed to be loud.
    /// </summary>
    private static int RunTrayCheck()
    {
        Console.WriteLine("Virtual KM Switch tray icon check");
        Console.WriteLine(new string('=', 66));

        TrayIcon icon;
        try
        {
            icon = new TrayIcon(
                "Virtual KM Switch spike",
                [
                    new TrayMenuItem(1, "Say hello"),
                    TrayMenuItem.Separator,
                    new TrayMenuItem(2, "Quit"),
                ]);
        }
        catch (Exception ex)
        {
            Console.WriteLine($"FAILED to add the icon: {ex.Message}");
            return 1;
        }

        Console.WriteLine("icon added. Look in the notification area (it may be under the overflow arrow).");
        Console.WriteLine("  left-click  -> reports activation");
        Console.WriteLine("  right-click -> shows the menu");
        Console.WriteLine();

        using var finished = new ManualResetEventSlim(false);
        using (icon)
        {
            icon.Activated += () => Console.WriteLine("activated (left click)");
            icon.CommandInvoked += command =>
            {
                Console.WriteLine($"menu command {command} chosen");
                if (command == 2)
                {
                    finished.Set();
                }
            };

            Console.WriteLine("waiting up to 60s; choose Quit from the menu to finish early.");
            finished.Wait(TimeSpan.FromSeconds(60));
        }

        Console.WriteLine("icon removed.");
        return 0;
    }

    /// <summary>
    /// Drives the full engagement path by hand: hotkeys, blocking, capture and panic.
    ///
    /// Nothing is sent anywhere -- captured events are printed -- so this shows exactly what would
    /// have gone on the wire, and what the local machine was prevented from seeing.
    /// </summary>
    private static int RunEngagementDemo()
    {
        long captured = 0;

        Console.WriteLine("Engagement demo -- 30 seconds.");
        Console.WriteLine();
        Console.WriteLine("  Ctrl+Alt+1          engage (keyboard and mouse go dead locally)");
        Console.WriteLine("  Ctrl+Alt+0          return control");
        Console.WriteLine("  Ctrl+Alt+Shift+Esc  panic");
        Console.WriteLine();
        Console.WriteLine("While engaged, typing and mouse movement are captured and shown here");
        Console.WriteLine("instead of reaching the desktop.");
        Console.WriteLine();

        // A hard backstop: blocking must never outlive this process, whatever else goes wrong.
        var watchdog = new Thread(() =>
        {
            Thread.Sleep(TimeSpan.FromSeconds(40));
            Environment.Exit(99);
        })
        { IsBackground = true };
        watchdog.Start();

        using var controller = new EngagementController(
            sink: (in InputEvent e) =>
            {
                long total = Interlocked.Increment(ref captured);

                // Only keys and buttons are printed: motion at kilohertz rates would bury them.
                if (e.Kind != InputEventKind.MouseMove && total < 200)
                {
                    Console.WriteLine($"    -> {e.Kind} code={e.Code} flags={e.Flags}");
                }
            },
            log: message => Console.WriteLine($"  [{message}]"));

        controller.StateChanged += (state, slot) =>
            Console.WriteLine($"  STATE: {state}{(slot > 0 ? $" -> slot {slot}" : string.Empty)}");
        controller.HostClaimRequested += () => Console.WriteLine("  HOST CLAIM requested");
        controller.PanicTriggered += () => Console.WriteLine("  PANIC handled");

        Thread.Sleep(TimeSpan.FromSeconds(30));

        Console.WriteLine();
        Console.WriteLine($"  captured {Interlocked.Read(ref captured)} events while engaged");
        Console.WriteLine($"  final state: {controller.State}");
        return 0;
    }

    /// <summary>
    /// Confirms the watchdog can actually tell a live hook from a dead one. A liveness probe that
    /// always answers "alive" is worse than no watchdog at all, so both directions are checked.
    /// </summary>
    private static int VerifyHookHealth()
    {
        Console.WriteLine();
        Console.WriteLine("Hook liveness watchdog:");

        var injector = new InputInjector();
        using var hook = new LowLevelHook();
        using var monitor = new HookHealthMonitor(hook, injector, log: m => Console.WriteLine($"    {m}"));

        HookProbeResult alive = monitor.Probe();
        Console.WriteLine($"  hook installed          : {alive}");

        // Disposing the hook is the closest available stand-in for Windows silently removing it.
        hook.Dispose();
        HookProbeResult dead = monitor.Probe();
        Console.WriteLine($"  hook disposed           : {dead}");

        bool aliveDetected = alive == HookProbeResult.Alive;
        bool deadDetected = dead == HookProbeResult.Dead;

        bool ok = aliveDetected && deadDetected;
        Console.WriteLine($"  => watchdog {(ok ? "distinguishes live from dead" : "IS NOT RELIABLE")}");
        return ok ? 0 : 1;
    }

    private static int VerifyMouseTravel(PointerBallistics ballistics, MouseCorrectionCurve correction)
    {
        var injector = new InputInjector();
        var calibration = new MouseCalibration(ballistics) { Correction = correction };

        (int originX, int originY) = Cursor.GetPosition();

        Console.WriteLine("Injecting horizontal moves and measuring actual cursor travel.");
        Console.WriteLine();
        Console.WriteLine("  requested | uncalibrated | calibrated | calibrated error");
        Console.WriteLine("  " + new string('-', 60));

        int failures = 0;

        foreach (int requested in new[] { 10, 25, 50, 100, 200, 400 })
        {
            int uncalibrated = MeasureUncalibrated(injector, requested);

            calibration.Reset();
            int calibrated = MeasureCalibrated(injector, calibration, requested);

            int error = calibrated - requested;

            // A little slack: the measurement races the input pipeline, and the cursor clamps at
            // screen edges.
            bool ok = Math.Abs(error) <= Math.Max(2, requested / 20);
            if (!ok)
            {
                failures++;
            }

            Console.WriteLine($"  {requested,9} | {uncalibrated,12} | {calibrated,10} | "
                + $"{error,4} {(ok ? "ok" : "FAIL")}");
        }

        Cursor.SetPosition(originX, originY);
        return failures;
    }

    private static int MeasureUncalibrated(InputInjector injector, int requested)
    {
        int start = BeginMeasure();

        Span<InputEvent> single = stackalloc InputEvent[1];
        single[0] = InputEvent.MouseMove(Stopwatch.GetTimestamp(), requested, 0);
        injector.Inject(single);

        return EndMeasure(start);
    }

    private static int MeasureCalibrated(InputInjector injector, MouseCalibration calibration, int requested)
    {
        int start = BeginMeasure();

        Span<InputEvent> chunks = stackalloc InputEvent[MouseCalibration.MaxChunksPerEvent];
        int count = calibration.Prepare(Stopwatch.GetTimestamp(), requested, 0, chunks);
        injector.Inject(chunks[..count]);

        return EndMeasure(start);
    }

    /// <summary>Parks the cursor well away from any screen edge, which would clamp travel.</summary>
    private static int BeginMeasure()
    {
        Cursor.SetPosition(400, 400);
        Thread.Sleep(60);
        return Cursor.GetPosition().X;
    }

    private static int EndMeasure(int start)
    {
        Thread.Sleep(120); // let the input pipeline settle
        return Cursor.GetPosition().X - start;
    }

    private static void VerifyKeyboardReplay()
    {
        Console.WriteLine();
        Console.WriteLine("Keyboard replay: focus a Notepad window within 5 seconds...");
        for (int i = 5; i > 0; i--)
        {
            Console.Write($"{i}... ");
            Thread.Sleep(1000);
        }

        Console.WriteLine();

        var injector = new InputInjector();

        // Scancodes for "virtual ok" typed on a US layout. Scancodes rather than virtual keys
        // is the whole point: the target reproduces the physical key, not a layout-dependent
        // character.
        ushort[] scanCodes = [0x2F, 0x17, 0x13, 0x14, 0x16, 0x1E, 0x26, 0x39, 0x18, 0x25];

        Span<InputEvent> batch = stackalloc InputEvent[2];
        foreach (ushort scanCode in scanCodes)
        {
            long now = Stopwatch.GetTimestamp();
            batch[0] = InputEvent.Key(now, scanCode, InputEventFlags.None);
            batch[1] = InputEvent.Key(now, scanCode, InputEventFlags.Up);
            injector.Inject(batch);
            Thread.Sleep(30);
        }

        Console.WriteLine("  Injected. Check whether the text appeared in Notepad.");
    }
}
