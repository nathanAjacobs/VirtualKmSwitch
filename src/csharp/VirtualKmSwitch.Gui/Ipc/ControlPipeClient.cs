using System.IO.Pipes;

namespace VirtualKmSwitch.Gui.Ipc;

/// <summary>
/// The GUI's end of the control channel.
///
/// Deliberately holds no authority of its own: it sends requests and the service decides what the
/// caller is allowed to do, from the caller's token. Nothing here can widen its own privileges.
/// </summary>
public sealed class ControlPipeClient : IAsyncDisposable
{
    private readonly SemaphoreSlim _oneRequestAtATime = new(1, 1);
    private readonly string _pipeName;
    private NamedPipeClientStream? _pipe;

    /// <param name="pipeName">Defaults to the production name; overridable to match a test server.</param>
    public ControlPipeClient(string? pipeName = null) => _pipeName = pipeName ?? ControlProtocol.PipeName;

    public bool IsConnected => _pipe?.IsConnected ?? false;

    public async Task ConnectAsync(TimeSpan timeout, CancellationToken cancellationToken = default)
    {
        await DisposePipeAsync().ConfigureAwait(false);

        var pipe = new NamedPipeClientStream(
            ".",
            _pipeName,
            PipeDirection.InOut,
            PipeOptions.Asynchronous,

            // Identification only. The service reads the caller's token to decide what it may do --
            // that is the whole authorisation model -- but may not act as the caller: this
            // connection is for asking the service to act, not for lending it an identity.
            System.Security.Principal.TokenImpersonationLevel.Identification);

        using var timeoutSource = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeoutSource.CancelAfter(timeout);

        await pipe.ConnectAsync(timeoutSource.Token).ConfigureAwait(false);
        _pipe = pipe;
    }

    /// <summary>Sends a request and waits for its response.</summary>
    public async Task<ControlResponse> SendAsync(
        ControlRequest request, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(request);

        NamedPipeClientStream pipe = _pipe
            ?? throw new InvalidOperationException("Not connected. Call ConnectAsync first.");

        // The pipe carries no request identifiers, so responses are matched to requests purely by
        // order. One in flight at a time keeps that true.
        await _oneRequestAtATime.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            await ControlProtocol.WriteAsync(pipe, request, cancellationToken).ConfigureAwait(false);

            return await ControlProtocol.ReadAsync<ControlResponse>(pipe, cancellationToken).ConfigureAwait(false)
                ?? ControlResponse.Fail("The service closed the connection without a usable response.");
        }
        finally
        {
            _oneRequestAtATime.Release();
        }
    }

    private async ValueTask DisposePipeAsync()
    {
        if (_pipe is not null)
        {
            await _pipe.DisposeAsync().ConfigureAwait(false);
            _pipe = null;
        }
    }

    public async ValueTask DisposeAsync()
    {
        await DisposePipeAsync().ConfigureAwait(false);
        _oneRequestAtATime.Dispose();
    }
}
