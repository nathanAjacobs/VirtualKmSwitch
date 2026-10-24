using Avalonia.Controls;
using Avalonia.Interactivity;
using VirtualKmSwitch.Gui.Ipc;
using VirtualKmSwitch.Gui.Services;

namespace VirtualKmSwitch.Gui.Pages;

/// <summary>
/// The controller's own preferences, and this machine's.
///
/// Appearance is per-user and stays in the controller. The focus decoy is a setting of this machine,
/// kept by the service and applied by its agents, but like hotkeys it affects nobody else's machine,
/// so changing it needs no privilege and asks for no confirmation.
/// </summary>
public partial class SettingsPage : UserControl
{
    /// <summary>The service's name for the focus decoy setting.</summary>
    private const string FocusDecoySetting = "focusDecoy";

    private readonly ServiceConnection _service;
    private readonly AppearanceStore _appearance;
    private readonly Dictionary<Button, ThemePreference> _segments;

    public SettingsPage()
        : this(new ServiceConnection(), new AppearanceStore())
    {
    }

    public SettingsPage(ServiceConnection service, AppearanceStore appearance)
    {
        InitializeComponent();
        _service = service;
        _appearance = appearance;

        _segments = new()
        {
            [SystemSegment] = ThemePreference.System,
            [LightSegment] = ThemePreference.Light,
            [DarkSegment] = ThemePreference.Dark,
        };

        Loaded += (_, _) =>
        {
            Show(_appearance.Load().Theme);
            _ = ReloadFocusDecoyAsync();
        };
    }

    private async Task ReloadFocusDecoyAsync()
    {
        ShowFocusDecoy(await _service.SendAsync(ControlVerb.GetSettings), "read");
    }

    private async void OnFocusDecoyClicked(object? sender, RoutedEventArgs e)
    {
        bool enabled = sender == FocusDecoyOnSegment;

        // Disabled while the change is in flight, so a second click cannot race the first.
        FocusDecoyOnSegment.IsEnabled = FocusDecoyOffSegment.IsEnabled = false;

        ControlResponse response = await _service.SendAsync(
            ControlVerb.SetSettings,
            new Dictionary<string, string> { [FocusDecoySetting] = enabled ? "true" : "false" });

        ShowFocusDecoy(response, "change");
    }

    /// <summary>
    /// Shows what the service says the setting is. It answers both reading and changing with the
    /// value now in force, so the buttons never show a choice that did not take.
    /// </summary>
    private void ShowFocusDecoy(ControlResponse response, string action)
    {
        if (!response.Success || !bool.TryParse(response.Values?.GetValueOrDefault(FocusDecoySetting), out bool enabled))
        {
            // Left unselected rather than guessed: showing "On" while the service is unreachable
            // would claim something nobody has checked. Retrying is a click away once it answers.
            FocusDecoyOnSegment.Classes.Set("selected", false);
            FocusDecoyOffSegment.Classes.Set("selected", false);
            FocusDecoyOnSegment.IsEnabled = FocusDecoyOffSegment.IsEnabled = true;
            FocusDecoyHintLabel.Text = $"Could not {action} this setting: {response.Error ?? "the service gave no value."}";
            return;
        }

        FocusDecoyOnSegment.Classes.Set("selected", enabled);
        FocusDecoyOffSegment.Classes.Set("selected", !enabled);
        FocusDecoyOnSegment.IsEnabled = FocusDecoyOffSegment.IsEnabled = true;

        FocusDecoyHintLabel.Text = enabled
            ? "On: games lose focus while you drive another machine."
            : "Off: the window you were using stays active while you drive another machine.";
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
