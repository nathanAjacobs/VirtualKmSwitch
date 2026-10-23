using VirtualKmSwitch.Gui.Ipc;

namespace VirtualKmSwitch.Gui.Services;

/// <summary>A snapshot of what the service reported.</summary>
public sealed record ServiceStatus
{
    public bool Reachable { get; init; }

    public string? Error { get; init; }

    /// <summary>Desktops with a running agent, e.g. "Default", "Winlogon".</summary>
    public IReadOnlyList<string> Agents { get; init; } = [];

    public long CapturedFromAgents { get; init; }

    public long DeliveredToAgents { get; init; }

    public static ServiceStatus Unreachable(string error) => new() { Reachable = false, Error = error };
}

/// <summary>
/// The controller's link to the service.
///
/// Everything the UI does goes through here, and nothing else in this process touches input or the
/// network. The service decides what the caller may do from the caller's own token, so this class
/// deliberately holds no authority — it cannot widen its own privileges by asking differently.
///
/// The connection is expected to come and go: the service restarts, an agent is respawned, the
/// machine sleeps. Reconnecting is routine, so a failure here is reported as status rather than
/// raised as an error.
/// </summary>
public sealed class ServiceConnection : IAsyncDisposable
{
    private static readonly TimeSpan ConnectTimeout = TimeSpan.FromSeconds(3);

    private readonly SemaphoreSlim _gate = new(1, 1);
    private readonly string? _pipeName;
    private ControlPipeClient? _client;

    /// <param name="pipeName">
    /// Defaults to the production pipe. Overridable so the controller can be pointed at a service
    /// run in development with <c>--pipe</c>, beside an installed one.
    /// </param>
    public ServiceConnection(string? pipeName = null) => _pipeName = pipeName;

    public bool IsConnected => _client?.IsConnected ?? false;

    /// <summary>The pipe this connection talks to, so an elevated request can go to the same one.</summary>
    public string PipeName => _pipeName ?? ControlProtocol.PipeName;

    /// <summary>Sends a request, connecting or reconnecting as needed.</summary>
    public async Task<ControlResponse> SendAsync(
        ControlVerb verb,
        Dictionary<string, string>? arguments = null,
        CancellationToken cancellationToken = default)
    {
        await _gate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            for (int attempt = 0; attempt < 2; attempt++)
            {
                try
                {
                    if (_client is null || !_client.IsConnected)
                    {
                        await ReconnectAsync(cancellationToken).ConfigureAwait(false);
                    }

                    return await _client!
                        .SendAsync(new ControlRequest { Verb = verb, Arguments = arguments }, cancellationToken)
                        .ConfigureAwait(false);
                }
                catch (Exception ex) when (attempt == 0)
                {
                    // One retry, because the common failure is a connection that died while idle
                    // rather than a service that is genuinely gone.
                    _ = ex;
                    await DropAsync().ConfigureAwait(false);
                }
            }

            return ControlResponse.Fail("The service is not reachable.");
        }
        catch (Exception ex)
        {
            await DropAsync().ConfigureAwait(false);
            return ControlResponse.Fail(ex.Message);
        }
        finally
        {
            _gate.Release();
        }
    }

    /// <summary>Asks the service how it is doing.</summary>
    public async Task<ServiceStatus> GetStatusAsync(CancellationToken cancellationToken = default)
    {
        ControlResponse response = await SendAsync(ControlVerb.GetStatus, cancellationToken: cancellationToken)
            .ConfigureAwait(false);

        if (!response.Success)
        {
            return ServiceStatus.Unreachable(response.Error ?? "unknown error");
        }

        Dictionary<string, string> values = response.Values ?? [];

        return new ServiceStatus
        {
            Reachable = true,
            Agents = values.GetValueOrDefault("agents", string.Empty)
                .Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries),
            CapturedFromAgents = Read(values, "capturedFromAgents"),
            DeliveredToAgents = Read(values, "deliveredToAgents"),
        };
    }

    private static long Read(Dictionary<string, string> values, string key) =>
        long.TryParse(values.GetValueOrDefault(key), out long parsed) ? parsed : 0;

    private async Task ReconnectAsync(CancellationToken cancellationToken)
    {
        await DropAsync().ConfigureAwait(false);

        var client = new ControlPipeClient(_pipeName);
        await client.ConnectAsync(ConnectTimeout, cancellationToken).ConfigureAwait(false);
        _client = client;
    }

    private async Task DropAsync()
    {
        if (_client is not null)
        {
            await _client.DisposeAsync().ConfigureAwait(false);
            _client = null;
        }
    }

    public async ValueTask DisposeAsync()
    {
        await DropAsync().ConfigureAwait(false);
        _gate.Dispose();
    }
}
