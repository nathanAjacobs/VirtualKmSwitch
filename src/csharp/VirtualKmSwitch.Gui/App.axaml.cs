using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using Avalonia.Threading;
using VirtualKmSwitch.Gui.Services;

namespace VirtualKmSwitch.Gui;

public partial class App : Application
{
    public override void Initialize() => AvaloniaXamlLoader.Load(this);

    public override void OnFrameworkInitializationCompleted()
    {
        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            // One connection shared by every page: reconnecting is routine, and several pages each
            // reconnecting independently would multiply that churn against the service.
            var service = new ServiceConnection();
            var appearance = new AppearanceStore();

            // Applied before the first window exists, so the controller opens in the chosen theme
            // rather than flashing the system one and correcting itself a moment later.
            AppearanceStore.Apply(appearance.Load().Theme);

            var window = new MainWindow(service, appearance);

            // One tray icon for the process, owned by the application rather than by a page: it has
            // to outlive the window being hidden, since keeping the controller alive with no window
            // is the whole point of having it.
            var tray = new TrayIconHost(service);
            tray.Attach(window, () => desktop.Shutdown());

            // Restart Manager brings back a controller that was only in the notification area the
            // same way, rather than opening a window nobody had open. Without an icon there would be
            // no way to reach a window that never opened, so that case opens it regardless.
            bool background = tray.IsAvailable
                && desktop.Args?.Contains(ApplicationRestart.BackgroundArgument) == true;

            // The lifetime shows the main window as it starts, so a background start has none. It
            // loses nothing: with the icon, closing only ever hides, and exiting is explicit.
            if (!background)
            {
                desktop.MainWindow = window;
            }

            desktop.ShutdownMode = ShutdownMode.OnMainWindowClose;
            desktop.Exit += (_, _) => tray.Dispose();

            ApplicationRestart.Register(windowVisible: !background);

            // Another launch, from the Start menu or the installer's finish page, opens this one.
            SingleInstance.Current?.Listen(() => Dispatcher.UIThread.Post(tray.ShowWindow));
        }

        base.OnFrameworkInitializationCompleted();
    }
}
