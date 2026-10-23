namespace VirtualKmSwitch.Gui.Services;

/// <summary>
/// Keeps one controller per sign-in session. Starting it again opens the one already running.
///
/// Two would mean two tray icons, and it happens without anyone meaning it to: an upgrade has
/// Restart Manager bring back the controller it closed, and then the installer's finish page offers
/// to open it too.
/// </summary>
/// <remarks>
/// A named event does both jobs. Whoever creates it is the running controller and waits on it; a
/// later launch finds it already there, signals it, and exits. Local\ makes it per session, so each
/// person signed in to the machine still gets a controller of their own.
/// </remarks>
public sealed class SingleInstance : IDisposable
{
    private const string EventName = @"Local\VirtualKmSwitch.Controller";

    private readonly EventWaitHandle _activate;
    private RegisteredWaitHandle? _wait;

    private SingleInstance(EventWaitHandle activate) => _activate = activate;

    /// <summary>This process's claim, or null when it made none (the previewer, the helper).</summary>
    public static SingleInstance? Current { get; private set; }

    /// <summary>
    /// Claims the session for this process, or, if another controller holds it, asks that one to
    /// show itself and returns null.
    /// </summary>
    public static SingleInstance? Claim()
    {
        var activate = new EventWaitHandle(false, EventResetMode.AutoReset, EventName, out bool createdNew);

        if (!createdNew)
        {
            activate.Set();
            activate.Dispose();
            return null;
        }

        Current = new SingleInstance(activate);
        return Current;
    }

    /// <summary>
    /// Calls <paramref name="activated"/>, on a thread-pool thread, each time another launch asks
    /// this controller to show itself.
    /// </summary>
    /// <remarks>
    /// A launch that came before this is not lost: the event stays signalled until it is waited on.
    /// </remarks>
    public void Listen(Action activated) =>
        _wait ??= ThreadPool.RegisterWaitForSingleObject(
            _activate, (_, _) => activated(), null, Timeout.Infinite, executeOnlyOnce: false);

    public void Dispose()
    {
        _wait?.Unregister(null);
        _wait = null;
        _activate.Dispose();
    }
}
