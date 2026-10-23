using System.Collections.ObjectModel;
using Avalonia.Controls;
using Avalonia.Interactivity;
using Avalonia.Threading;
using VirtualKmSwitch.Gui.Ipc;
using VirtualKmSwitch.Gui.Services;

namespace VirtualKmSwitch.Gui.Pages;

/// <summary>A paired machine as the list shows it.</summary>
public sealed record MachineRow(Guid NodeId, string DisplayName, string Detail, string Hotkey)
{
    /// <summary>The letter shown in the row's tile, so a list of machines can be scanned.</summary>
    public string Initial => string.IsNullOrWhiteSpace(DisplayName)
        ? "?"
        : DisplayName.TrimStart()[..1].ToUpperInvariant();

    /// <summary>Whether to show the hotkey pill at all; an empty one would be a floating outline.</summary>
    public bool HasHotkey => Hotkey.Length > 0;
}

/// <summary>
/// Pairing: showing a code, entering one, and seeing which machines this one trusts.
///
/// Everything here that changes trust is administrator-tier, because a paired machine can type into
/// this one. The controller itself stays unelevated: those requests go through <see cref="Elevation"/>,
/// which asks Windows for administrator rights for that one change. The page says so before asking,
/// so the prompt reads as the design working rather than as a surprise.
/// </summary>
public partial class MachinesPage : UserControl
{
    private readonly ServiceConnection _service;
    private readonly ObservableCollection<MachineRow> _machines = [];

    private DispatcherTimer? _countdown;
    private DateTime _pairingClosesAt;
    private int _ticks;
    private HashSet<Guid> _pairedBeforeWindow = [];

    public MachinesPage()
        : this(new ServiceConnection())
    {
    }

    public MachinesPage(ServiceConnection service)
    {
        InitializeComponent();
        _service = service;
        MachineList.ItemsSource = _machines;

        Loaded += (_, _) => _ = RefreshAsync();

        // Leaving the page does not close pairing on the service, but the code is no longer on
        // screen, so there is no point counting it down here.
        Unloaded += (_, _) => HidePairing();
    }

    private async void OnShowCodeClicked(object? sender, RoutedEventArgs e)
    {
        // Off while the UAC prompt is up, so a second click cannot raise a second prompt behind it.
        ShowCodeButton.IsEnabled = false;
        ControlResponse response = await Elevation.SendAsync(_service, ControlVerb.OpenPairingWindow);

        if (!response.Success)
        {
            ShowCodeButton.IsEnabled = true;
            Report(response, "show a pairing code");
            return;
        }

        // The code comes back exactly once, in the response to the act that created it, so it is
        // shown from here rather than fetched again later.
        string? code = response.Values?.GetValueOrDefault("code");
        long seconds = long.TryParse(response.Values?.GetValueOrDefault("closesInSeconds"), out long parsed) ? parsed : 300;

        if (string.IsNullOrEmpty(code))
        {
            ShowCodeButton.IsEnabled = true;
            ShowMessage("The service opened pairing but did not return a code.");
            return;
        }

        MachinesView? view = MachinesView.FromResponse(response);
        _pairedBeforeWindow = [.. view?.Machines.Where(machine => !machine.IsLocal).Select(machine => machine.NodeId) ?? []];

        MessageLabel.IsVisible = false;
        ShowPairing(code, TimeSpan.FromSeconds(seconds));
    }

    private async void OnCancelPairingClicked(object? sender, RoutedEventArgs e)
    {
        HidePairing();
        ControlResponse response = await _service.SendAsync(ControlVerb.ClosePairingWindow);

        if (!response.Success)
        {
            Report(response, "close pairing");
        }
    }

    private async void OnEnterCodeClicked(object? sender, RoutedEventArgs e)
    {
        string? code = await Dialogs.PromptAsync(
            this,
            "Enter a pairing code",
            "Type the code shown on the other machine. Spaces, dashes and case do not matter.",
            placeholder: "XXXX-XXXX-XXXX-XXXX",
            maxLength: 24);

        if (string.IsNullOrWhiteSpace(code))
        {
            return;
        }

        EnterCodeButton.IsEnabled = false;
        ShowMessage("Looking for the machine showing that code…");

        try
        {
            ControlResponse response = await PairAsync(code.Trim(), host: null);

            // The machine showing the code is normally found by its announcement on the network.
            // Some networks drop those, so offer the address route instead of a dead end.
            if (!response.Success && response.Error?.Contains("No machine on this network", StringComparison.Ordinal) == true)
            {
                string? host = await Dialogs.PromptAsync(
                    this,
                    "Machine not found",
                    "It may be on a network that blocks discovery. Enter its name or IP address to try directly.",
                    placeholder: "192.168.1.20",
                    maxLength: 253);

                if (string.IsNullOrWhiteSpace(host))
                {
                    Report(response, "pair");
                    return;
                }

                response = await PairAsync(code.Trim(), host.Trim());
            }

            if (!response.Success)
            {
                Report(response, "pair");
                return;
            }

            string name = response.Values?.GetValueOrDefault("pairedName") ?? "the other machine";
            ShowMessage($"Paired with {name}.");
            await RefreshAsync();
        }
        finally
        {
            EnterCodeButton.IsEnabled = true;
        }
    }

    private Task<ControlResponse> PairAsync(string code, string? host)
    {
        var arguments = new Dictionary<string, string> { ["code"] = code };
        if (host is not null)
        {
            arguments["host"] = host;
        }

        return Elevation.SendAsync(_service, ControlVerb.PairWithCode, arguments);
    }

    private async void OnRemoveClicked(object? sender, RoutedEventArgs e)
    {
        if (sender is not Button { Tag: Guid nodeId })
        {
            return;
        }

        MachineRow? row = _machines.FirstOrDefault(machine => machine.NodeId == nodeId);

        bool confirmed = await Dialogs.ConfirmAsync(
            this,
            "Remove this machine?",
            $"This machine stops trusting '{row?.DisplayName ?? nodeId.ToString()}', and any hotkey pointing at "
            + "it becomes unassigned. To pair them again, show a new code.",
            "Remove",
            "Cancel");

        if (!confirmed)
        {
            return;
        }

        ControlResponse response = await Elevation.SendAsync(
            _service,
            ControlVerb.RemovePeer,
            new Dictionary<string, string> { ["nodeId"] = nodeId.ToString() });

        if (!response.Success)
        {
            Report(response, "remove that machine");
            return;
        }

        ShowMessage($"Removed {row?.DisplayName ?? "the machine"}.");
        await RefreshAsync();
    }

    /// <summary>
    /// Reports a failure. A refusal for want of rights has already been answered with a UAC prompt by
    /// the time it gets here, so what is left is a declined prompt or a genuine fault, and the
    /// service's own words say which.
    /// </summary>
    private void Report(ControlResponse response, string action) =>
        ShowMessage($"Could not {action}: {response.Error}");

    private void ShowMessage(string text)
    {
        MessageLabel.Text = text;
        MessageLabel.IsVisible = true;
    }

    private void ShowPairing(string code, TimeSpan duration)
    {
        PairingCodeLabel.Text = code;
        PairingPanel.IsVisible = true;
        ShowCodeButton.IsEnabled = false;
        _pairingClosesAt = DateTime.UtcNow + duration;
        _ticks = 0;

        _countdown?.Stop();
        _countdown = new DispatcherTimer { Interval = TimeSpan.FromSeconds(1) };
        _countdown.Tick += async (_, _) => await TickAsync();
        _countdown.Start();

        UpdateCountdown();
    }

    private void HidePairing()
    {
        // The code disappears from the screen when it stops working, so the display can never
        // suggest a code that would now be rejected.
        _countdown?.Stop();
        PairingPanel.IsVisible = false;
        PairingCodeLabel.Text = string.Empty;
        ShowCodeButton.IsEnabled = true;
    }

    private async Task TickAsync()
    {
        UpdateCountdown();

        // Every other second, ask whether the other machine has paired: the window closes itself on
        // success, and the person watching should see that without having to refresh.
        if (++_ticks % 2 != 0 || !PairingPanel.IsVisible)
        {
            return;
        }

        ControlResponse response = await _service.SendAsync(ControlVerb.GetMachines);
        MachinesView? view = MachinesView.FromResponse(response);

        if (view is null || view.PairingOpen)
        {
            return;
        }

        HidePairing();

        MachineInfo? joined = view.Machines.FirstOrDefault(
            machine => !machine.IsLocal && !_pairedBeforeWindow.Contains(machine.NodeId));

        ShowMessage(joined is null
            ? "Pairing closed without a machine joining."
            : $"Paired with {joined.Label}.");

        Show(view, await ReadHotkeySlotsAsync());
    }

    private void UpdateCountdown()
    {
        TimeSpan remaining = _pairingClosesAt - DateTime.UtcNow;

        if (remaining <= TimeSpan.Zero)
        {
            HidePairing();
            ShowMessage("The pairing code expired. Show a new one to try again.");
            return;
        }

        PairingCountdownLabel.Text = $"Closes in {remaining.Minutes:0}:{remaining.Seconds:00}";
    }

    private async Task RefreshAsync()
    {
        ControlResponse response = await _service.SendAsync(ControlVerb.GetMachines);

        if (!response.Success)
        {
            _machines.Clear();
            MachinesEmptyLabel.IsVisible = false;
            ThisMachineLabel.Text = $"Paired machines could not be read: {response.Error}";
            SetButtons(enabled: false);
            return;
        }

        MachinesView? view = MachinesView.FromResponse(response);
        if (view is null)
        {
            _machines.Clear();
            MachinesEmptyLabel.IsVisible = false;
            ThisMachineLabel.Text = "The service answered, but its reply could not be understood.";
            SetButtons(enabled: false);
            return;
        }

        SetButtons(enabled: true);
        Show(view, await ReadHotkeySlotsAsync());
    }

    private void Show(MachinesView view, Dictionary<Guid, int> slots)
    {
        MachineInfo? local = view.Machines.FirstOrDefault(machine => machine.IsLocal);
        int paired = view.Machines.Count(machine => !machine.IsLocal);

        ThisMachineLabel.Text = local is null
            ? $"Paired with {paired} machine(s)."
            : $"This machine is '{local.Label}', paired with {paired} other machine(s).";

        ShowHealth(view);

        _machines.Clear();
        foreach (MachineInfo machine in view.Machines.Where(machine => !machine.IsLocal))
        {
            string hotkey = slots.TryGetValue(machine.NodeId, out int slot) ? $"Ctrl+Alt+{slot}" : string.Empty;
            _machines.Add(new MachineRow(machine.NodeId, machine.Label, Detail(machine), hotkey));
        }

        MachinesEmptyLabel.IsVisible = _machines.Count == 0;
    }

    /// <summary>
    /// The identifying facts, short enough to read but long enough to tell two machines apart.
    /// </summary>
    private static string Detail(MachineInfo machine)
    {
        string thumbprint = machine.Thumbprint.Length >= 16 ? machine.Thumbprint[..16] : machine.Thumbprint;
        string state = machine.IsConnected ? "connected" : "not connected";
        return $"{state}  ·  {machine.NodeId:N}  ·  cert {thumbprint}…";
    }

    /// <summary>
    /// This machine's own hotkey bindings, so each row can show the key that drives it.
    /// </summary>
    /// <remarks>
    /// A failure here is not worth reporting: the bindings have their own page, and losing the column
    /// is better than replacing a readable list with an error.
    /// </remarks>
    private async Task<Dictionary<Guid, int>> ReadHotkeySlotsAsync()
    {
        var slots = new Dictionary<Guid, int>();

        ControlResponse response = await _service.SendAsync(ControlVerb.GetHotkeys);
        if (!response.Success || response.Values is null)
        {
            return slots;
        }

        foreach ((string key, string value) in response.Values)
        {
            if (int.TryParse(key, out int slot) && Guid.TryParse(value, out Guid target))
            {
                slots[target] = slot;
            }
        }

        return slots;
    }

    private void ShowHealth(MachinesView view)
    {
        HealthLabel.IsVisible = view.Health != TrustHealth.Ok;

        HealthLabel.Text = view.Health switch
        {
            // Worth saying out loud: this list decides who may type into this machine, and a machine
            // quietly running on a recovered or empty one is not a detail.
            TrustHealth.RecoveredFromBackup =>
                "The list of paired machines was damaged and the backup was used. Check that every machine "
                + $"you expect is listed below. ({view.HealthDetail})",

            TrustHealth.Unreadable =>
                "Neither the list of paired machines nor its backup could be read. Machines must be paired "
                + $"again. ({view.HealthDetail})",

            _ => string.Empty,
        };
    }

    private void SetButtons(bool enabled)
    {
        ShowCodeButton.IsEnabled = enabled && !PairingPanel.IsVisible;
        EnterCodeButton.IsEnabled = enabled;
    }
}
