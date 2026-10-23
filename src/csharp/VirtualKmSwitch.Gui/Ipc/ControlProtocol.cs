using System.Buffers.Binary;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace VirtualKmSwitch.Gui.Ipc;

/// <summary>
/// Operations the controller can ask the service to perform.
///
/// The names are the wire format: the service (src/rust/crates/vkms-ipc/src/protocol.rs) reads them as
/// strings, and answers a name it does not know with a refusal rather than a dropped connection.
/// Which verbs need administrator rights is decided by the service, from the caller's own token;
/// nothing here can change it.
/// </summary>
public enum ControlVerb
{
    Unknown = 0,

    // --- Available to the interactive user ---
    GetStatus,
    Engage,
    Disengage,
    GetHotkeys,
    SetHotkeys,
    GetMachines,

    // Closing pairing early only narrows who may pair, so it needs no rights.
    ClosePairingWindow,

    // --- Require administrator rights: they change whom this machine trusts ---
    OpenPairingWindow,
    PairWithCode,
    RemovePeer,
}

public sealed record ControlRequest
{
    public ControlVerb Verb { get; init; }

    /// <summary>Free-form arguments, interpreted per verb.</summary>
    public Dictionary<string, string>? Arguments { get; init; }
}

public sealed record ControlResponse
{
    public bool Success { get; init; }
    public string? Error { get; init; }
    public Dictionary<string, string>? Values { get; init; }

    public static ControlResponse Ok(Dictionary<string, string>? values = null) =>
        new() { Success = true, Values = values };

    public static ControlResponse Fail(string error) => new() { Success = false, Error = error };

    /// <summary>
    /// Whether the service turned this down because the caller is not an administrator, rather than
    /// because the operation itself failed. Matches the refusal in src/rust/crates/vkms-ipc/src/pipe_server.rs.
    /// </summary>
    [JsonIgnore]
    public bool NeedsAdministrator =>
        !Success && Error?.EndsWith("requires administrator rights.", StringComparison.Ordinal) == true;
}

/// <summary>How requests are framed on the wire: a 4-byte little-endian length, then JSON.</summary>
public static class ControlProtocol
{
    /// <summary>Refuse anything larger; a control message is small and this bounds a hostile peer.</summary>
    public const int MaxMessageBytes = 64 * 1024;

    /// <summary>The pipe the service listens on.</summary>
    public const string PipeName = "VirtualKmSwitch.Control";

    public static JsonSerializerOptions JsonOptions { get; } = new()
    {
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
        Converters = { new JsonStringEnumConverter() },
    };

    /// <summary>Writes one framed message.</summary>
    public static async Task WriteAsync<T>(Stream stream, T message, CancellationToken cancellationToken)
    {
        byte[] payload = JsonSerializer.SerializeToUtf8Bytes(message, JsonOptions);

        byte[] header = new byte[4];
        BinaryPrimitives.WriteInt32LittleEndian(header, payload.Length);

        await stream.WriteAsync(header, cancellationToken).ConfigureAwait(false);
        await stream.WriteAsync(payload, cancellationToken).ConfigureAwait(false);
        await stream.FlushAsync(cancellationToken).ConfigureAwait(false);
    }

    /// <summary>
    /// Reads one framed message: null when the other end closed first, or sent something that is
    /// not a message of this protocol.
    /// </summary>
    public static async Task<T?> ReadAsync<T>(Stream stream, CancellationToken cancellationToken)
        where T : class
    {
        byte[] header = new byte[4];
        if (!await ReadExactlyAsync(stream, header, cancellationToken).ConfigureAwait(false))
        {
            return null;
        }

        int length = BinaryPrimitives.ReadInt32LittleEndian(header);
        if (length <= 0 || length > MaxMessageBytes)
        {
            return null;
        }

        byte[] payload = new byte[length];
        if (!await ReadExactlyAsync(stream, payload, cancellationToken).ConfigureAwait(false))
        {
            return null;
        }

        return JsonSerializer.Deserialize<T>(payload, JsonOptions);
    }

    private static async Task<bool> ReadExactlyAsync(
        Stream stream, Memory<byte> buffer, CancellationToken cancellationToken)
    {
        int read = 0;
        while (read < buffer.Length)
        {
            int chunk = await stream.ReadAsync(buffer[read..], cancellationToken).ConfigureAwait(false);
            if (chunk == 0)
            {
                return false;
            }

            read += chunk;
        }

        return true;
    }
}
