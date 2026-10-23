using System.Diagnostics;
using System.Runtime.InteropServices;
using VirtualKmSwitch.Core;
using VirtualKmSwitch.Interop;

namespace VirtualKmSwitch.UacSpike;

/// <summary>
/// Proves (or disproves) the premise of the three-tier process model: that a LocalSystem service
/// can place a helper inside the interactive session, and that the helper can follow the input
/// desktop onto Winlogon and inject there -- i.e. drive a live UAC prompt.
///
/// Two modes in one executable:
///   --service : runs as LocalSystem in session 0, spawns the agent into the console session.
///   --agent   : runs as SYSTEM inside that session, follows the desktop, injects on Winlogon.
///
/// Neither mode has a console, so both log to a file.
///
/// The injected key is ESCAPE, which *dismisses* a UAC prompt. Choosing Escape rather than
/// Enter keeps the spike from approving an elevation it did not intend to: if the prompt
/// disappears, injection into the secure desktop demonstrably works, and nothing was granted.
/// </summary>
internal static class Program
{
    private static readonly string LogDirectory =
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.CommonApplicationData), "VirtualKmSwitch");

    private static readonly string _logPath = Path.Combine(LogDirectory, "uac-spike.log");

    /// <summary>Used if ProgramData is not writable by whatever identity we ended up running as.</summary>
    private static readonly string FallbackLogPath =
        Path.Combine(Path.GetTempPath(), "vkms-uac-spike.log");
    private static readonly Lock LogGate = new();

    private static int Main(string[] args)
    {
        // Logged before anything else can fail. Under Task Scheduler there is no console, so a
        // process that starts and then exits down some other path is indistinguishable from one
        // that never started at all -- unless it says so here first.
        Log($"=== process start === argc={args.Length} args=[{string.Join(" | ", args)}] "
            + $"cwd={Environment.CurrentDirectory}");

        string mode = args.FirstOrDefault() ?? "--help";

        return mode switch
        {
            "--service" => RunService(),
            "--agent" => RunAgent(),
            "--agent-secure" => RunSecureDesktopAgent(),
            "--tail" => TailLog(),
            _ => ShowHelp(),
        };
    }

    private static int ShowHelp()
    {
        Console.WriteLine("Virtual KM Switch UAC spike");
        Console.WriteLine();
        Console.WriteLine("  --service   run as LocalSystem (session 0); spawns the agent");
        Console.WriteLine("  --agent     run inside the console session as SYSTEM");
        Console.WriteLine("  --tail      print the log written by the other two modes");
        Console.WriteLine();
        Console.WriteLine($"Log: {_logPath}");
        return 1;
    }

    private static int TailLog()
    {
        if (!File.Exists(_logPath))
        {
            Console.WriteLine($"No log yet at {_logPath}");
            return 1;
        }

        foreach (string line in File.ReadAllLines(_logPath))
        {
            Console.WriteLine(line);
        }

        return 0;
    }

    // ---------------------------------------------------------------------------------------
    // Service side: session 0. Cannot inject anything itself; its whole job is placing the agent.
    // ---------------------------------------------------------------------------------------
    private static int RunService()
    {
        Log("=== service starting ===");
        Log($"identity: {Environment.UserName}, session: {Process.GetCurrentProcess().SessionId}");

        try
        {
            uint session = SessionLauncher.GetActiveConsoleSession();
            Log($"active console session: {session}");

            if (session == SessionLauncher.NoSession)
            {
                Log("no interactive session attached; nothing to do");
                return 1;
            }

            string executable = Environment.ProcessPath
                ?? throw new InvalidOperationException("Cannot determine own executable path.");

            uint defaultPid = SessionLauncher.LaunchAsSystemInSession(
                executable, "--agent", session, @"Winsta0\Default");
            Log($"spawned Default-desktop agent as SYSTEM in session {session}, pid {defaultPid}");

            // The interesting one: created directly on the secure desktop rather than migrating
            // onto it later. SetThreadDesktop moved a thread there successfully but SendInput was
            // still denied, which points at the process's own desktop association mattering.
            try
            {
                uint securePid = SessionLauncher.LaunchAsSystemInSession(
                    executable, "--agent-secure", session, @"Winsta0\Winlogon");
                Log($"spawned Winlogon-desktop agent as SYSTEM in session {session}, pid {securePid}");
            }
            catch (Exception ex)
            {
                Log($"could not spawn agent on Winlogon desktop: {ex.Message}");
            }

            return 0;
        }
        catch (Exception ex)
        {
            Log($"SERVICE FAILED: {ex}");
            return 2;
        }
    }

    // ---------------------------------------------------------------------------------------
    // Agent side: inside the interactive session, as SYSTEM. This is the half that can inject.
    // ---------------------------------------------------------------------------------------
    private static int RunAgent()
    {
        Log("=== agent starting ===");
        Log($"identity: {Environment.UserName}, session: {Process.GetCurrentProcess().SessionId}");
        using (var identity = System.Security.Principal.WindowsIdentity.GetCurrent())
        {
            Log($"is SYSTEM: {identity.IsSystem}, sid: {identity.User}");
        }

        var injector = new InputInjector();
        int secureDesktopVisits = 0;

        try
        {
            using var watcher = new DesktopWatcher(
                onDesktopChanged: name =>
                {
                    Log($"input desktop is now '{name}'");

                    if (!string.Equals(name, "Winlogon", StringComparison.OrdinalIgnoreCase))
                    {
                        return;
                    }

                    secureDesktopVisits++;
                    Log($"SECURE DESKTOP reached (visit {secureDesktopVisits}) -- injecting ESC");

                    // Injection must happen on the thread that owns the desktop attachment,
                    // which is exactly the thread this callback runs on.
                    long now = Stopwatch.GetTimestamp();
                    Span<InputEvent> escape =
                    [
                        InputEvent.Key(now, 0x01, InputEventFlags.None),
                        InputEvent.Key(now, 0x01, InputEventFlags.Up),
                    ];

                    uint accepted = injector.Inject(escape);
                    int error = Marshal.GetLastWin32Error();

                    // SendInput reports UIPI blocking neither in its return value nor in
                    // GetLastError, so 'accepted' is necessary but not sufficient evidence.
                    // Whether the prompt actually closed is the real result.
                    Log($"  SendInput accepted {accepted}/2 events (last error {error})");
                },
                log: Log);

            Log("watching for desktop switches for 120s -- trigger a UAC prompt now");

            for (int i = 0; i < 120; i++)
            {
                Thread.Sleep(1000);
            }

            Log($"=== agent finished; secure desktop reached {secureDesktopVisits} time(s) ===");
            return secureDesktopVisits > 0 ? 0 : 1;
        }
        catch (Exception ex)
        {
            Log($"AGENT FAILED: {ex}");
            return 2;
        }
    }

    /// <summary>
    /// Runs already attached to the Winlogon desktop, having been created there. It therefore never
    /// calls SetThreadDesktop -- doing so is what the previous attempt did, and injection was
    /// denied. It only watches for its own desktop becoming the active input desktop.
    /// </summary>
    private static int RunSecureDesktopAgent()
    {
        Log("=== secure-desktop agent starting ===");
        Log($"identity: {Environment.UserName}, session: {Process.GetCurrentProcess().SessionId}");

        using (var identity = System.Security.Principal.WindowsIdentity.GetCurrent())
        {
            Log($"is SYSTEM: {identity.IsSystem}");
        }

        var injector = new InputInjector();
        string previous = string.Empty;
        int injections = 0;

        try
        {
            for (int tick = 0; tick < 1200; tick++)
            {
                string current = DesktopWatcher.GetInputDesktopName();

                if (current != previous)
                {
                    Log($"input desktop is now '{current}'");
                    previous = current;

                    if (string.Equals(current, "Winlogon", StringComparison.OrdinalIgnoreCase))
                    {
                        injections++;
                        Log($"SECURE DESKTOP active (visit {injections}) -- running injection sequence");

                        // The desktop switches BEFORE consent.exe has created and focused its
                        // dialog, so injecting immediately can land before anything exists to
                        // receive it. Several attempts are spread over a few seconds, and both
                        // encodings are tried, so a null result means "rejected" rather than
                        // "mistimed" or "wrong encoding".
                        InjectAfter(injector, 750, "scancode", scanCode: true);
                        InjectAfter(injector, 750, "virtual-key", scanCode: false);
                        InjectAfter(injector, 1000, "scancode (late)", scanCode: true);
                        InjectAfter(injector, 1000, "virtual-key (late)", scanCode: false);
                    }
                }

                Thread.Sleep(100);
            }

            Log($"=== secure agent finished; {injections} injection attempt(s) ===");
            return 0;
        }
        catch (Exception ex)
        {
            Log($"SECURE AGENT FAILED: {ex}");
            return 2;
        }
    }

    /// <summary>
    /// Waits, then injects Escape once, reporting exactly what SendInput said.
    /// </summary>
    private static void InjectAfter(InputInjector injector, int delayMilliseconds, string label, bool scanCode)
    {
        Thread.Sleep(delayMilliseconds);

        const ushort EscapeScanCode = 0x01;
        const ushort VkEscape = 0x1B;

        long now = Stopwatch.GetTimestamp();
        Span<InputEvent> escape = scanCode
            ?
            [
                InputEvent.Key(now, EscapeScanCode, InputEventFlags.None),
                InputEvent.Key(now, EscapeScanCode, InputEventFlags.Up),
            ]
            :
            [
                InputEvent.Key(now, VkEscape, InputEventFlags.VirtualKey),
                InputEvent.Key(now, VkEscape, InputEventFlags.VirtualKey | InputEventFlags.Up),
            ];

        uint accepted = injector.Inject(escape);
        int error = Marshal.GetLastWin32Error();
        Log($"  [{label}] on '{DesktopWatcher.GetInputDesktopName()}': "
            + $"SendInput accepted {accepted}/2 (last error {error})");
    }

    private static void Log(string message)
    {
        string line = $"{DateTime.Now:HH:mm:ss.fff} [{Environment.ProcessId,6}] {message}";
        lock (LogGate)
        {
            foreach (string path in new[] { _logPath, FallbackLogPath })
            {
                try
                {
                    Directory.CreateDirectory(Path.GetDirectoryName(path)!);
                    File.AppendAllText(path, line + Environment.NewLine);
                    return;
                }
                catch
                {
                    // Fall through to the next candidate. A spike must never die because logging
                    // failed, but it must also not fail *silently* -- hence the second location.
                }
            }
        }
    }
}
