using System.Collections.ObjectModel;
using Avalonia.Controls;
using Avalonia.Interactivity;
using VirtualKmSwitch.Gui.Services;
using VirtualKmSwitch.Gui.Ipc;

namespace VirtualKmSwitch.Gui.Pages;

/// <summary>One bindable slot, as the list shows it.</summary>
public sealed record SlotRow(int Slot, string Chord, string Assignment, bool IsAssigned)
{
    /// <summary>Unassigned slots are dimmed, so the bound ones read at a glance.</summary>
    public double Opacity => IsAssigned ? 1.0 : 0.55;
}

/// <summary>
/// Editor for this machine's hotkey bindings.
///
/// Bindings are local, so nothing here affects another machine — which is why this is a runtime
/// operation the interactive user can perform without administrator rights, unlike pairing, which
/// changes whom this machine trusts.
///
/// The fixed keys are shown but not editable. Return-to-local and panic are how you get back when
/// something is wrong, and a rebindable escape hatch is one that can be misplaced.
/// </summary>
public partial class HotkeysPage : UserControl
{
    private readonly ServiceConnection _service;
    private readonly ObservableCollection<SlotRow> _slots = [];

    /// <summary>Paired machines: driving yourself is not a thing to bind a key to.</summary>
    private IReadOnlyList<MachineInfo> _assignable = [];

    public HotkeysPage()
        : this(new ServiceConnection())
    {
    }

    public HotkeysPage(ServiceConnection service)
    {
        InitializeComponent();
        _service = service;
        SlotsView.ItemsSource = _slots;

        Loaded += (_, _) => _ = ReloadAsync();
    }

    private async Task ReloadAsync()
    {
        // Machines first: a slot's binding is a node id, and turning that into a name people
        // recognise is the whole reason this page reads membership at all.
        IReadOnlyList<MachineInfo> machines = await ReadMachinesAsync();
        _assignable = [.. machines.Where(machine => !machine.IsLocal)];

        Dictionary<Guid, MachineInfo> byId = machines.ToDictionary(machine => machine.NodeId);

        ControlResponse response = await _service.SendAsync(ControlVerb.GetHotkeys);

        _slots.Clear();

        if (!response.Success)
        {
            MessageLabel.Text = $"Could not read hotkeys: {response.Error}";
            MessageLabel.IsVisible = true;
            return;
        }

        ShowGuidance(machines);
        Dictionary<string, string> values = response.Values ?? [];

        for (int slot = 1; slot <= 9; slot++)
        {
            string target = values.GetValueOrDefault(slot.ToString(), string.Empty);
            bool assigned = Guid.TryParse(target, out Guid nodeId) && nodeId != Guid.Empty;

            string assignment;
            if (!assigned)
            {
                assignment = "unassigned";
            }
            else if (byId.TryGetValue(nodeId, out MachineInfo? machine))
            {
                assignment = machine.Label;
            }
            else
            {
                // A binding whose machine is not paired should not exist — the service clears them
                // on removal — so seeing one means the two disagree, and saying so is better than
                // showing a bare id that looks like a machine.
                assignment = $"{nodeId:N} — no longer paired";
            }

            _slots.Add(new SlotRow(slot, $"Ctrl+Alt+{slot}", assignment, assigned));
        }
    }

    private void ShowGuidance(IReadOnlyList<MachineInfo> machines)
    {
        if (machines.Count == 0)
        {
            MessageLabel.Text = "The paired machines could not be read, so there is nothing to show yet.";
            MessageLabel.IsVisible = true;
            return;
        }

        if (_assignable.Count == 0)
        {
            MessageLabel.Text =
                "This machine is not paired with any other yet, so there is nothing to switch to. "
                + "Pair one on the Machines page.";
            MessageLabel.IsVisible = true;
            return;
        }

        MessageLabel.IsVisible = false;
    }

    private async Task<IReadOnlyList<MachineInfo>> ReadMachinesAsync()
    {
        ControlResponse response = await _service.SendAsync(ControlVerb.GetMachines);
        return response.Success ? MachinesView.FromResponse(response)?.Machines ?? [] : [];
    }

    private async void OnAssignClicked(object? sender, RoutedEventArgs e)
    {
        if (sender is not Button { Tag: int slot })
        {
            return;
        }

        if (_assignable.Count == 0)
        {
            MessageLabel.Text = "There is no paired machine to assign.";
            MessageLabel.IsVisible = true;
            return;
        }

        string[] choices = [.. _assignable.Select(machine => machine.Label)];

        string? chosen = await Dialogs.ChooseAsync(this, $"Ctrl+Alt+{slot} drives…", "Cancel", choices);

        MachineInfo? machine = _assignable.FirstOrDefault(candidate => candidate.Label == chosen);
        if (machine is null)
        {
            return;
        }

        await BindAsync(slot, machine.NodeId.ToString(), $"assign slot {slot}");
    }

    private async void OnClearClicked(object? sender, RoutedEventArgs e)
    {
        if (sender is not Button { Tag: int slot })
        {
            return;
        }

        // Clearing sends an empty target rather than a separate verb: unbinding is an ordinary
        // thing to want, and a second verb would only be a second way to get it wrong.
        await BindAsync(slot, string.Empty, $"clear slot {slot}");
    }

    private async Task BindAsync(int slot, string target, string action)
    {
        ControlResponse response = await _service.SendAsync(
            ControlVerb.SetHotkeys,
            new Dictionary<string, string>
            {
                ["slot"] = slot.ToString(),
                ["target"] = target,
            });

        if (!response.Success)
        {
            MessageLabel.Text = $"Could not {action}: {response.Error}";
            MessageLabel.IsVisible = true;
            return;
        }

        await ReloadAsync();
    }

    private async void OnReloadClicked(object? sender, RoutedEventArgs e)
    {
        ReloadButton.IsEnabled = false;
        try
        {
            await ReloadAsync();
        }
        finally
        {
            ReloadButton.IsEnabled = true;
        }
    }
}
