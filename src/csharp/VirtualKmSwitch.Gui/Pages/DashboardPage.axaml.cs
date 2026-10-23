using Avalonia.Controls;
using Avalonia.Interactivity;
using Avalonia.Threading;
using VirtualKmSwitch.Gui.Ipc;
using VirtualKmSwitch.Gui.Services;

namespace VirtualKmSwitch.Gui.Pages;

/// <summary>
/// The controller's main view: is the service healthy, are both agents up, and is input flowing.
///
/// Deliberately reports the things that fail invisibly. A missing Winlogon agent, for instance,
/// changes nothing a user would notice until the moment a UAC prompt appears on a remote machine and
/// cannot be answered — so it is stated here rather than inferred from an empty list.
/// </summary>
public partial class DashboardPage : UserControl
{
    private static readonly TimeSpan PollInterval = TimeSpan.FromSeconds(2);

    private readonly ServiceConnection _service;
    private CancellationTokenSource? _polling;

    public DashboardPage()
        : this(new ServiceConnection())
    {
    }

    public DashboardPage(ServiceConnection service)
    {
        InitializeComponent();
        _service = service;

        Loaded += (_, _) =>
        {
            _polling = new CancellationTokenSource();
            _ = PollAsync(_polling.Token);
        };

        // Polling stops with the page: a controller sitting in the tray should not keep waking the
        // service to ask questions nobody is reading.
        Unloaded += (_, _) =>
        {
            _polling?.Cancel();
            _polling?.Dispose();
            _polling = null;
        };
    }

    private async Task PollAsync(CancellationToken cancellationToken)
    {
        while (!cancellationToken.IsCancellationRequested)
        {
            await RefreshAsync(cancellationToken);

            try
            {
                await Task.Delay(PollInterval, cancellationToken);
            }
            catch (OperationCanceledException)
            {
                return;
            }
        }
    }

    private async Task RefreshAsync(CancellationToken cancellationToken)
    {
        ServiceStatus status = await _service.GetStatusAsync(cancellationToken);

        if (!cancellationToken.IsCancellationRequested)
        {
            await Dispatcher.UIThread.InvokeAsync(() => Apply(status));
        }
    }

    private void Apply(ServiceStatus status)
    {
        if (!status.Reachable)
        {
            SetState(ServiceIndicator, "negative");
            ServiceStatusLabel.Text = "Not running";
            ServiceHintLabel.Text =
                "Install and start the service, then this will connect automatically.\n" + $"({status.Error})";
            ServiceHintLabel.IsVisible = true;

            SetState(DefaultAgentDot, "negative");
            SetState(WinlogonAgentDot, "negative");

            CapturedLabel.Text = "—";
            DeliveredLabel.Text = "—";
            return;
        }

        ServiceHintLabel.IsVisible = false;

        bool hasDefault = status.Agents.Contains("Default", StringComparer.OrdinalIgnoreCase);
        bool hasWinlogon = status.Agents.Contains("Winlogon", StringComparer.OrdinalIgnoreCase);

        if (hasDefault && hasWinlogon)
        {
            SetState(ServiceIndicator, "positive");
            ServiceStatusLabel.Text = "Running";
        }
        else
        {
            // Degraded rather than broken: input still works, but something specific is missing,
            // and saying which is the difference between a fixable report and a mystery.
            SetState(ServiceIndicator, "warning");
            ServiceStatusLabel.Text = "Running, but not all agents are up";
        }

        SetState(DefaultAgentDot, hasDefault ? "positive" : "negative");
        SetState(WinlogonAgentDot, hasWinlogon ? "positive" : "negative");

        AgentsHintLabel.Text = hasWinlogon
            ? "One agent runs per desktop. The secure-desktop agent is what answers UAC prompts."
            : "The secure-desktop agent is not running: UAC prompts on this machine cannot be answered remotely.";

        CapturedLabel.Text = $"{status.CapturedFromAgents:N0}";
        DeliveredLabel.Text = $"{status.DeliveredToAgents:N0}";
    }

    /// <summary>The state is a class, so the theme decides the colour and repaints it on change.</summary>
    private static void SetState(Border dot, string state)
    {
        foreach (string candidate in new[] { "positive", "warning", "negative" })
        {
            dot.Classes.Set(candidate, candidate == state);
        }
    }

    private async void OnRefreshClicked(object? sender, RoutedEventArgs e)
    {
        RefreshButton.IsEnabled = false;
        try
        {
            await RefreshAsync(CancellationToken.None);
        }
        finally
        {
            RefreshButton.IsEnabled = true;
        }
    }

    private async void OnReturnControlClicked(object? sender, RoutedEventArgs e)
    {
        // The visible equivalent of the panic hotkey: a way back that does not depend on the
        // keyboard, which is the thing that may be misbehaving.
        ControlResponse response = await _service.SendAsync(ControlVerb.Disengage);

        LastActionLabel.Text = response.Success
            ? "Control returned to this machine."
            : $"Could not return control: {response.Error}";

        LastActionLabel.IsVisible = true;
    }
}
