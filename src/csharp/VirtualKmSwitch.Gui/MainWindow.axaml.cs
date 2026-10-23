using Avalonia;
using Avalonia.Controls;
using Avalonia.Interactivity;
using VirtualKmSwitch.Gui.Pages;
using VirtualKmSwitch.Gui.Services;

namespace VirtualKmSwitch.Gui;

/// <summary>
/// The controller's one window: the navigation rail and whichever page is showing.
/// </summary>
public partial class MainWindow : Window
{
    /// <summary>Width of the toggle plus its margin, when there is no rail to match.</summary>
    private const double ToggleOnlyWidth = 50;

    private readonly Dictionary<Button, Func<Control>> _destinations;
    private readonly Dictionary<Button, Control> _created = [];

    // Parameterless constructor for the XAML loader and previewer only.
    public MainWindow()
        : this(new ServiceConnection(), new AppearanceStore())
    {
    }

    public MainWindow(ServiceConnection service, AppearanceStore appearance)
    {
        InitializeComponent();

        // Pages are made the first time they are visited and then kept, so returning to one does
        // not lose what it was showing. Each starts and stops its own polling on Loaded/Unloaded,
        // which fire as it is swapped in and out.
        _destinations = new()
        {
            [StatusNav] = () => new DashboardPage(service),
            [MachinesNav] = () => new MachinesPage(service),
            [HotkeysNav] = () => new HotkeysPage(service),
            [SettingsNav] = () => new SettingsPage(appearance),
        };

        Navigate(StatusNav);
    }

    /// <summary>
    /// Refuses fullscreen, however it was asked for.
    /// </summary>
    /// <remarks>
    /// The caption no longer offers it, but Avalonia takes the capability from the platform rather
    /// than from the window, so there is no setting to turn it off; putting the window back is the
    /// one guard that covers every route in.
    /// </remarks>
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);

        if (change.Property == WindowStateProperty && WindowState == WindowState.FullScreen)
        {
            WindowState previous = change.GetOldValue<WindowState>();
            WindowState = previous == WindowState.FullScreen ? WindowState.Normal : previous;
        }
    }

    private void OnNavClicked(object? sender, RoutedEventArgs e)
    {
        if (sender is Button destination)
        {
            Navigate(destination);
        }
    }

    private void Navigate(Button destination)
    {
        if (!_created.TryGetValue(destination, out Control? page))
        {
            page = _destinations[destination]();
            _created[destination] = page;
        }

        PageHost.Content = page;

        // Driven from the navigation that actually happened rather than from the click, so the rail
        // cannot end up highlighting a page that is not the one showing.
        foreach (Button item in _destinations.Keys)
        {
            item.Classes.Set("selected", ReferenceEquals(item, destination));
        }
    }

    /// <summary>
    /// Shows or hides the navigation rail.
    /// </summary>
    /// <remarks>
    /// The toggle belongs to the caption, not to the rail, which is the whole point: a toggle inside
    /// the thing it collapses would disappear along with it. So the rail collapses to just the
    /// toggle's width and lets the page colour through behind it.
    /// </remarks>
    private void OnPaneToggleClicked(object? sender, RoutedEventArgs e) => SetRailOpen(!IsRailOpen);

    public bool IsRailOpen => Wordmark.IsVisible;

    private void SetRailOpen(bool open)
    {
        Wordmark.IsVisible = open;
        Destinations.IsVisible = open;
        RailFooter.IsVisible = open;

        Rail.Width = open ? (double)this.FindResource("RailWidth")! : ToggleOnlyWidth;

        // A class rather than colours set here, so the theme still decides what "open" looks like.
        Rail.Classes.Set("collapsed", !open);
    }
}
