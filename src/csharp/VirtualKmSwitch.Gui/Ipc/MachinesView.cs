using System.Text.Json;

namespace VirtualKmSwitch.Gui.Ipc;

/// <summary>One machine, as the controller needs to show it.</summary>
public sealed record MachineInfo
{
    public Guid NodeId { get; init; }

    /// <summary>What the machine calls itself.</summary>
    public string DisplayName { get; init; } = string.Empty;

    /// <summary>SHA-256 of its certificate: the identity everything else authenticates against.</summary>
    public string Thumbprint { get; init; } = string.Empty;

    public bool IsLocal { get; init; }

    /// <summary>Whether there is a live link to it right now. Always false for this machine.</summary>
    public bool IsConnected { get; init; }

    /// <summary>
    /// A name to show when the machine has not named itself.
    /// </summary>
    /// <remarks>
    /// Falls back to a short form of the node id rather than to something invented like "Machine 2".
    /// A number that looks like a name is worse than an obvious identifier, because it reads as
    /// information when it is really just a position in a list.
    /// </remarks>
    public string Label => string.IsNullOrWhiteSpace(DisplayName)
        ? NodeId.ToString("N")[..12]
        : DisplayName;
}

/// <summary>Why the paired-machine list on disk could not be read normally.</summary>
public enum TrustHealth
{
    /// <summary>Read normally, or there was nothing to read yet.</summary>
    Ok,

    /// <summary>The primary file was damaged and the backup was used.</summary>
    RecoveredFromBackup,

    /// <summary>Neither copy was readable. Machines must be paired again.</summary>
    Unreadable,
}

/// <summary>
/// This machine and every machine it is paired with, in one round trip.
///
/// Pairing is local: each machine keeps its own list and nothing is shared beyond the moment of
/// pairing, so this is the whole of what this machine knows about the others.
/// </summary>
public sealed record MachinesView
{
    public Guid LocalNodeId { get; init; }

    public TrustHealth Health { get; init; } = TrustHealth.Ok;

    /// <summary>Set when <see cref="Health"/> is not Ok.</summary>
    public string? HealthDetail { get; init; }

    /// <summary>This machine first, then every machine it is paired with.</summary>
    public IReadOnlyList<MachineInfo> Machines { get; init; } = [];

    /// <summary>Whether this machine is currently accepting a pairing, and for how much longer.</summary>
    public bool PairingOpen { get; init; }

    public long PairingSecondsLeft { get; init; }

    /// <summary>The response key this travels under.</summary>
    public const string ValueKey = "machines";

    /// <summary>
    /// Decodes a view from a response, or null if it carries none -- never an empty list in place
    /// of a missing one, since "nothing paired" and "the service did not answer" look identical on
    /// screen and only one of them is true.
    /// </summary>
    public static MachinesView? FromResponse(ControlResponse response)
    {
        if (response.Values?.TryGetValue(ValueKey, out string? json) != true || string.IsNullOrEmpty(json))
        {
            return null;
        }

        try
        {
            return JsonSerializer.Deserialize<MachinesView>(json, ControlProtocol.JsonOptions);
        }
        catch (JsonException)
        {
            return null;
        }
    }
}
