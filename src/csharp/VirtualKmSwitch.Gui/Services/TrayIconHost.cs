using Avalonia.Controls;
using Avalonia.Threading;
using VirtualKmSwitch.Gui.Native;
using TrayIcon = VirtualKmSwitch.Gui.Native.TrayIcon;

namespace VirtualKmSwitch.Gui.Services;

/// <summary>
/// Keeps the controller in the notification area, and keeps it running when its window is closed.
///
/// Closing the window hides it rather than exiting, which is the behaviour a status tool wants: the
/// controller is a place to look at the machines, not a thing you finish using. Exiting is still
/// available from the menu, and costs nothing — the service and its agents are separate processes
/// and go on working with no controller running at all.
///
/// Only a person closing the window hides it. Windows closing the controller, to end the session or
/// so an installer can replace it, closes it for real: refusing would veto a sign-out, and leaves an
/// upgrade running the old controller until something kills it.
/// </summary>
public sealed class TrayIconHost : IDisposable
{
    private const uint CommandOpen = 1;
    private const uint CommandExit = 2;

    /// <summary>Slow on purpose: this only decides what a tooltip says.</summary>
    private static readonly TimeSpan TooltipPollInterval = TimeSpan.FromSeconds(10);

    private readonly ServiceConnection _service;

    private TrayIcon? _icon;
    private Window? _window;
    private Action? _exit;
    private CancellationTokenSource? _polling;
    private bool _disposed;

    public TrayIconHost(ServiceConnection service) => _service = service;

    /// <summary>Whether there is an icon to come back from; without one, closing the window exits.</summary>
    public bool IsAvailable => _icon is not null;

    /// <summary>
    /// Binds the tray icon to the application's window.
    /// </summary>
    /// <param name="exit">Ends the application; called when Exit is chosen from the menu.</param>
    /// <remarks>
    /// A failure here is swallowed deliberately. The tray icon is a convenience, and an application
    /// that refuses to start because the notification area misbehaved would be trading a small loss
    /// for a total one. Without an icon, closing the window simply exits.
    /// </remarks>
    public void Attach(Window window, Action exit)
    {
        _window = window;
        _exit = exit;

        // Consulted on every close rather than once, so a deliberate exit can let the same close
        // through. Every close that is not the person's own arrives as a shutdown: Exit from the
        // menu, and Avalonia answering WM_QUERYENDSESSION, both for the session ending and for
        // Restart Manager, which sends it with ENDSESSION_CLOSEAPP.
        window.Closing += (_, args) =>
        {
            if (_icon is not null && args.CloseReason == WindowCloseReason.WindowClosing)
            {
                args.Cancel = true;
                window.Hide();
                ApplicationRestart.Register(windowVisible: false);
            }
        };

        try
        {
            _icon = new TrayIcon(
                "Virtual KM Switch",
                [
                    new TrayMenuItem(CommandOpen, "Open Virtual KM Switch"),
                    TrayMenuItem.Separator,
                    new TrayMenuItem(CommandExit, "Exit"),
                ]);
        }
        catch (Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"tray icon unavailable: {ex.Message}");
            return;
        }

        // Both events arrive on the tray icon's own thread, so nothing here may touch the UI
        // without hopping to the UI thread first.
        _icon.Activated += () => Dispatcher.UIThread.Post(ShowWindow);
        _icon.CommandInvoked += command => Dispatcher.UIThread.Post(() => Invoke(command));

        _polling = new CancellationTokenSource();
        _ = PollTooltipAsync(_polling.Token);
    }

    /// <summary>Brings the window back, from the notification area or from being minimised.</summary>
    public void ShowWindow()
    {
        if (_window is null)
        {
            return;
        }

        _window.Show();

        // A window hidden while minimised would otherwise come back still minimised, so clicking the
        // tray icon would appear to do nothing.
        if (_window.WindowState == WindowState.Minimized)
        {
            _window.WindowState = WindowState.Normal;
        }

        _window.Activate();
        ApplicationRestart.Register(windowVisible: true);
    }

    private void Invoke(uint command)
    {
        switch (command)
        {
            case CommandOpen:
                ShowWindow();
                break;

            case CommandExit:
                // Removed before quitting: an icon left behind lingers in the tray until the user
                // happens to hover over it, which looks like the application is still running.
                Dispose();
                _exit?.Invoke();
                break;
        }
    }

    private async Task PollTooltipAsync(CancellationToken cancellationToken)
    {
        while (!cancellationToken.IsCancellationRequested)
        {
            ServiceStatus status = await _service.GetStatusAsync(cancellationToken).ConfigureAwait(false);

            _icon?.SetTooltip(status.Reachable
                ? $"Virtual KM Switch — service running ({status.Agents.Count} agent(s))"
                : "Virtual KM Switch — service not running");

            try
            {
                await Task.Delay(TooltipPollInterval, cancellationToken).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                return;
            }
        }
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;

        _polling?.Cancel();
        _polling?.Dispose();
        _polling = null;

        _icon?.Dispose();
        _icon = null;
    }
}
