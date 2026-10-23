using Avalonia;
using VirtualKmSwitch.Gui.Services;

namespace VirtualKmSwitch.Gui;

internal static class Program
{
    [STAThread]
    public static int Main(string[] args) =>
        args is [Elevation.HelperArgument, string channel, string servicePipe]
            ? Elevation.RunHelper(channel, servicePipe)
            : RunController(args);

    private static int RunController(string[] args)
    {
        // A second controller in the same session only asks the first to show itself.
        using SingleInstance? instance = SingleInstance.Claim();
        return instance is null ? 0 : BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);
    }

    /// <summary>Also used by the Avalonia previewer, which is why it is public and separate.</summary>
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<App>()
            .UsePlatformDetect()
            .LogToTrace();
}
