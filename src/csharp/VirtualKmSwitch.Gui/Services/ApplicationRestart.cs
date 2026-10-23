using Windows.Win32;
using Windows.Win32.Foundation;
using Windows.Win32.System.Recovery;

namespace VirtualKmSwitch.Gui.Services;

/// <summary>
/// Asks Windows to start the controller again after Restart Manager closes it.
///
/// That is what installing Virtual KM Switch over itself does: Windows Installer has Restart Manager close
/// whatever holds the files it is about to replace, then start again whatever registered here. A
/// controller that did not register would simply be gone after an upgrade, with nothing to say so.
/// </summary>
public static class ApplicationRestart
{
    /// <summary>Starts the controller in the notification area, without opening its window.</summary>
    public const string BackgroundArgument = "--background";

    /// <summary>
    /// Registers how the controller should come back: with its window open, or only in the
    /// notification area. Called again whenever that changes, since the last registration wins.
    /// </summary>
    /// <remarks>
    /// Not after a crash or a hang: Windows Error Reporting would otherwise restart a controller
    /// that keeps failing, and the service does not need one running. Reboots are left allowed, so
    /// Windows' own "restart apps when I sign in" brings it back the same way.
    /// </remarks>
    public static void Register(bool windowVisible)
    {
        HRESULT result = PInvoke.RegisterApplicationRestart(
            windowVisible ? null : BackgroundArgument,
            REGISTER_APPLICATION_RESTART_FLAGS.RESTART_NO_CRASH | REGISTER_APPLICATION_RESTART_FLAGS.RESTART_NO_HANG);

        // Swallowed like the tray icon's failures: coming back after an upgrade is a convenience,
        // and the controller works the same without it.
        if (result.Failed)
        {
            System.Diagnostics.Debug.WriteLine($"restart registration failed: {result}");
        }
    }
}
