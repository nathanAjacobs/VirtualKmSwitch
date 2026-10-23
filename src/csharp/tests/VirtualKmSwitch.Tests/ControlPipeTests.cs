using VirtualKmSwitch.Ipc;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class ControlProtocolTests
{
    [Theory]
    [InlineData(ControlVerb.GetStatus)]
    [InlineData(ControlVerb.Engage)]
    [InlineData(ControlVerb.Disengage)]
    [InlineData(ControlVerb.SetHotkeys)]
    [InlineData(ControlVerb.ClaimHost)]
    public void RuntimeVerbsAreAvailableToTheInteractiveUser(ControlVerb verb) =>
        Assert.Equal(ControlTier.Runtime, ControlProtocol.TierFor(verb));

    [Theory]
    [InlineData(ControlVerb.CreateCluster)]
    [InlineData(ControlVerb.JoinCluster)]
    [InlineData(ControlVerb.LeaveCluster)]
    [InlineData(ControlVerb.EnrollPeer)]
    [InlineData(ControlVerb.RemovePeer)]
    [InlineData(ControlVerb.OpenPairingWindow)]
    public void MembershipChangesRequireAdministrator(ControlVerb verb) =>
        Assert.Equal(ControlTier.Administrative, ControlProtocol.TierFor(verb));

    [Fact]
    public void UnclassifiedVerbsDefaultToAdministrative()
    {
        // A verb added later without being classified must fail closed. The alternative — silently
        // exposing a new operation to any local user — is the dangerous direction.
        Assert.Equal(ControlTier.Administrative, ControlProtocol.TierFor(ControlVerb.Unknown));
        Assert.Equal(ControlTier.Administrative, ControlProtocol.TierFor((ControlVerb)9999));
    }
}

/// <summary>
/// End-to-end over a real named pipe: a real server, a real client, real ACLs. Only the caller's
/// privilege is simulated, since a test process cannot drop to a different token on demand.
/// </summary>
public class ControlPipeServerTests
{
    /// <summary>
    /// A unique pipe per test. Using the production name would attach to a VirtualKmSwitch service if
    /// one happens to be installed on the machine, and the tests would then silently exercise that
    /// instead — passing or failing for reasons entirely unrelated to the code under test.
    /// </summary>
    private static string UniquePipeName([System.Runtime.CompilerServices.CallerMemberName] string caller = "") =>
        $"VirtualKmSwitch.Test.{caller}.{Environment.ProcessId}";

    private static async Task<ControlPipeClient> ConnectAsync(string pipeName)
    {
        var client = new ControlPipeClient(pipeName);
        await client.ConnectAsync(TimeSpan.FromSeconds(5));
        return client;
    }

    [Fact]
    public async Task RequestsRoundTripOverTheRealPipe()
    {
        string pipeName = UniquePipeName();
        await using var server = new ControlPipeServer(
            (request, _) => ValueTask.FromResult(ControlResponse.Ok(new Dictionary<string, string>
            {
                ["echo"] = request.Verb.ToString(),
            })),
            pipeName: pipeName);

        await using ControlPipeClient client = await ConnectAsync(pipeName);

        ControlResponse response = await client.SendAsync(new ControlRequest { Verb = ControlVerb.GetStatus });

        Assert.True(response.Success);
        Assert.Equal("GetStatus", response.Values?["echo"]);
    }

    [Fact]
    public async Task ArgumentsSurviveTheRoundTrip()
    {
        Dictionary<string, string>? seen = null;

        string pipeName = UniquePipeName();
        await using var server = new ControlPipeServer(
            (request, _) =>
            {
                seen = request.Arguments;
                return ValueTask.FromResult(ControlResponse.Ok());
            },
            pipeName: pipeName);

        await using ControlPipeClient client = await ConnectAsync(pipeName);

        await client.SendAsync(new ControlRequest
        {
            Verb = ControlVerb.Engage,
            Arguments = new Dictionary<string, string> { ["slot"] = "3" },
        });

        Assert.Equal("3", seen?["slot"]);
    }

    [Fact]
    public async Task SeveralRequestsShareOneConnection()
    {
        string pipeName = UniquePipeName();
        await using var server = new ControlPipeServer(
            (request, _) => ValueTask.FromResult(ControlResponse.Ok(new Dictionary<string, string>
            {
                ["verb"] = request.Verb.ToString(),
            })),
            pipeName: pipeName);

        await using ControlPipeClient client = await ConnectAsync(pipeName);

        // Responses are matched to requests by order alone, so this is what proves the framing
        // stays aligned across messages.
        foreach (ControlVerb verb in new[] { ControlVerb.GetStatus, ControlVerb.Engage, ControlVerb.Disengage })
        {
            ControlResponse response = await client.SendAsync(new ControlRequest { Verb = verb });
            Assert.Equal(verb.ToString(), response.Values?["verb"]);
        }
    }

    [Fact]
    public async Task AHandlerThatThrowsBecomesAFailureNotADroppedConnection()
    {
        string pipeName = UniquePipeName();
        await using var server = new ControlPipeServer(
            (_, _) => throw new InvalidOperationException("handler exploded"),
            pipeName: pipeName);

        await using ControlPipeClient client = await ConnectAsync(pipeName);

        ControlResponse response = await client.SendAsync(new ControlRequest { Verb = ControlVerb.GetStatus });

        Assert.False(response.Success);
        Assert.Contains("handler exploded", response.Error);
    }

    [Fact]
    public async Task TheHandlerIsToldWhichTierTheCallerHas()
    {
        ControlTier? observed = null;

        string pipeName = UniquePipeName();
        await using var server = new ControlPipeServer(
            (_, tier) =>
            {
                observed = tier;
                return ValueTask.FromResult(ControlResponse.Ok());
            },
            pipeName: pipeName);

        await using ControlPipeClient client = await ConnectAsync(pipeName);
        await client.SendAsync(new ControlRequest { Verb = ControlVerb.GetStatus });

        // The value depends on whether the test process is elevated; what matters is that the
        // server determined it from the connection rather than from anything the client sent.
        Assert.NotNull(observed);
    }
}
