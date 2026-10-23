using Avalonia.Controls;
using Avalonia.Interactivity;
using VirtualKmSwitch.Gui.Services;

namespace VirtualKmSwitch.Gui.Pages;

/// <summary>
/// The controller's own preferences.
///
/// Everything here is per-user and local. Nothing on this page reaches the service or any other
/// machine, which is why it needs no privilege and asks for no confirmation.
/// </summary>
public partial class SettingsPage : UserControl
{
    private readonly AppearanceStore _appearance;
    private readonly Dictionary<Button, ThemePreference> _segments;

    public SettingsPage()
        : this(new AppearanceStore())
    {
    }

    public SettingsPage(AppearanceStore appearance)
    {
        InitializeComponent();
        _appearance = appearance;

        _segments = new()
        {
            [SystemSegment] = ThemePreference.System,
            [LightSegment] = ThemePreference.Light,
            [DarkSegment] = ThemePreference.Dark,
        };

        Loaded += (_, _) => Show(_appearance.Load().Theme);
    }

    private void OnSegmentClicked(object? sender, RoutedEventArgs e)
    {
        if (sender is not Button segment || !_segments.TryGetValue(segment, out ThemePreference preference))
        {
            return;
        }

        // Applied before saving, so a failure to write leaves the window looking like what the user
        // just picked rather than silently ignoring them.
        AppearanceStore.Apply(preference);
        _appearance.Save(_appearance.Load() with { Theme = preference });

        Show(preference);
    }

    private void Show(ThemePreference selected)
    {
        // Colours come from the style for the "selected" class, so this follows theme changes --
        // including Windows changing underneath "System" -- without repainting anything here.
        foreach ((Button segment, ThemePreference preference) in _segments)
        {
            segment.Classes.Set("selected", preference == selected);
        }

        ThemeHintLabel.Text = selected switch
        {
            ThemePreference.System => "Following Windows, including when it changes.",
            ThemePreference.Light => "Always light, whatever Windows is set to.",
            _ => "Always dark, whatever Windows is set to.",
        };
    }
}
