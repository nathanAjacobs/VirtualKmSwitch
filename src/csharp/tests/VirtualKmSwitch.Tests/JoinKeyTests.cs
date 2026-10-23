using VirtualKmSwitch.Ipc;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class JoinKeyTests
{
    [Fact]
    public void AKeyIsGroupedForReadingAloud()
    {
        string key = JoinKey.Create();

        Assert.Equal("XXXX-XXXX-XXXX-XXXX".Length, key.Length);
        Assert.Equal(4, key.Split('-').Length);
        Assert.All(key.Split('-'), group => Assert.Equal(4, group.Length));
    }

    [Theory]
    [InlineData('I')]
    [InlineData('O')]
    [InlineData('0')]
    [InlineData('1')]
    public void TheAlphabetOmitsCharactersThatAreMisreadOffAScreen(char excluded)
    {
        // Someone is going to read this out loud to someone in another room. A key containing
        // both O and 0 turns that into a guessing game, and the failure looks like a wrong key.
        for (int attempt = 0; attempt < 200; attempt++)
        {
            Assert.DoesNotContain(excluded, JoinKey.Create());
        }
    }

    [Fact]
    public void KeysDoNotRepeat()
    {
        var seen = new HashSet<string>();

        for (int attempt = 0; attempt < 500; attempt++)
        {
            Assert.True(seen.Add(JoinKey.Create()));
        }
    }

    [Theory]
    [InlineData("ABCD-EFGH-JKLM-NPQR")]
    [InlineData("abcd-efgh-jklm-npqr")]
    [InlineData("ABCDEFGHJKLMNPQR")]
    [InlineData("ABCD EFGH JKLM NPQR")]
    [InlineData("  ABCD-efgh JKLM-npqr  ")]
    public void TranscriptionDifferencesDoNotChangeTheKey(string typed)
    {
        // Case, spaces and missing dashes are how people actually type a key they were told over
        // the phone; none of them should be the difference between joining and being refused.
        Assert.Equal("ABCDEFGHJKLMNPQR", JoinKey.Normalize(typed));
    }

    [Fact]
    public void GenuinelyDifferentKeysStayDifferent()
    {
        Assert.NotEqual(JoinKey.Normalize("ABCD-EFGH-JKLM-NPQR"), JoinKey.Normalize("ABCD-EFGH-JKLM-NPQS"));
    }

    [Fact]
    public void EmptyInputNormalisesToEmptyRatherThanThrowing()
    {
        Assert.Equal(string.Empty, JoinKey.Normalize(string.Empty));
        Assert.Equal(string.Empty, JoinKey.Normalize("   "));
    }

    [Fact]
    public void TheClusterIdIsStableAcrossHowTheKeyWasTyped()
    {
        // The beacon advertises this id, so two machines holding the same key must derive the
        // same one regardless of who typed it and how.
        string fromCreator = EnrollmentHandshake.ClusterIdFor(JoinKey.Normalize("ABCD-EFGH-JKLM-NPQR"));
        string fromJoiner = EnrollmentHandshake.ClusterIdFor(JoinKey.Normalize("abcd efgh jklm npqr"));

        Assert.Equal(fromCreator, fromJoiner);
    }
}

public class ClusterViewTests
{
    [Fact]
    public void AMachineWithNoNameFallsBackToItsIdRatherThanAnInventedName()
    {
        // "Machine 2" would read as information when it is really just a position in a list.
        var machine = new MachineInfo { NodeId = Guid.NewGuid() };

        Assert.Equal(machine.NodeId.ToString("N")[..12], machine.Label);
    }

    [Fact]
    public void ANamedMachineShowsItsName()
    {
        var machine = new MachineInfo { NodeId = Guid.NewGuid(), DisplayName = "Workshop" };

        Assert.Equal("Workshop", machine.Label);
    }

    [Fact]
    public void AViewSurvivesTheRoundTripThroughAControlResponse()
    {
        var view = new ClusterView
        {
            HasCluster = true,
            ClusterId = "0123456789ABCDEF",
            ClusterName = "Home",
            LocalNodeId = Guid.NewGuid(),
            Health = ClusterHealth.RecoveredFromBackup,
            HealthDetail = "primary was empty",
            Machines = [new MachineInfo { NodeId = Guid.NewGuid(), DisplayName = "Desk", IsLocal = true }],
        };

        ControlResponse response = ControlResponse.Ok(new Dictionary<string, string>
        {
            [ClusterView.ValueKey] = view.ToJson(),
        });

        ClusterView? decoded = ClusterView.FromResponse(response);

        Assert.NotNull(decoded);
        Assert.Equal(view.ClusterName, decoded.ClusterName);
        Assert.Equal(view.LocalNodeId, decoded.LocalNodeId);
        Assert.Equal(ClusterHealth.RecoveredFromBackup, decoded.Health);
        Assert.Equal("Desk", Assert.Single(decoded.Machines).DisplayName);
    }

    [Fact]
    public void TheJoinKeyIsAbsentUnlessDeliberatelyIncluded()
    {
        ClusterView view = new() { HasCluster = true };

        Assert.Null(ClusterView.FromResponse(
            ControlResponse.Ok(new Dictionary<string, string> { [ClusterView.ValueKey] = view.ToJson() }))?.JoinKey);
    }

    [Fact]
    public void AResponseCarryingNoViewDecodesToNothingRatherThanAnEmptyCluster()
    {
        // An empty cluster and "the service did not answer that question" look identical on
        // screen, and only one of them means every peer has left.
        Assert.Null(ClusterView.FromResponse(ControlResponse.Ok()));
        Assert.Null(ClusterView.FromResponse(ControlResponse.Fail("nope")));
    }

    [Fact]
    public void MalformedContentDecodesToNothingRatherThanThrowing()
    {
        ControlResponse response = ControlResponse.Ok(new Dictionary<string, string>
        {
            [ClusterView.ValueKey] = "{ not json",
        });

        Assert.Null(ClusterView.FromResponse(response));
    }
}
