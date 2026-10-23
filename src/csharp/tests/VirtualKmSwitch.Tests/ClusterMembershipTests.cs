using VirtualKmSwitch.Cluster;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class ClusterMembershipTests : IDisposable
{
    private const string Thumbprint = "AABBCCDDEEFF00112233445566778899AABBCCDDEEFF001122334455667788AA";

    private readonly string _directory =
        Path.Combine(Path.GetTempPath(), "VirtualKmSwitchTests", Guid.NewGuid().ToString("N"));

    private ClusterStore Store() => new(Path.Combine(_directory, "roster.json"));

    private LocalSettingsStore Settings() => new(Path.Combine(_directory, "local.json"));

    private ClusterMembership Create() => new(Store(), Settings());

    private ClusterMembership CreateWithCluster()
    {
        ClusterMembership membership = Create();
        membership.CreateCluster("Home", "0123456789ABCDEF", "AAAA-BBBB-CCCC-DDDD", Thumbprint, "Desk");
        return membership;
    }

    [Fact]
    public void AFreshMachineBelongsToNoCluster()
    {
        ClusterMembership membership = Create();

        Assert.False(membership.HasCluster);
        Assert.Empty(membership.Machines);
        Assert.Equal(LoadOutcome.Absent, membership.LoadOutcome);
    }

    [Fact]
    public void AMachineGetsOneIdentityAndKeepsIt()
    {
        // The node id is what every peer pins a certificate against, so a machine that generated
        // a new one on each start would look like a different computer after every reboot.
        Guid first = Create().LocalNodeId;
        Guid second = Create().LocalNodeId;

        Assert.NotEqual(Guid.Empty, first);
        Assert.Equal(first, second);
    }

    [Fact]
    public void CreatingAClusterYieldsAOneMemberRosterContainingThisMachine()
    {
        ClusterMembership membership = CreateWithCluster();

        RosterEntry only = Assert.Single(membership.Machines);

        Assert.Equal(membership.LocalNodeId, only.NodeId);
        Assert.Equal("Desk", only.DisplayName);
        Assert.Equal(Thumbprint, only.Thumbprint);
        Assert.True(membership.HasCluster);
    }

    [Fact]
    public void MembershipSurvivesARestart()
    {
        // The point of persisting it: whoever becomes host after a cold boot uses the last saved
        // membership rather than reconstructing it from whoever happens to be online.
        Guid nodeId = CreateWithCluster().LocalNodeId;

        ClusterMembership reloaded = Create();

        Assert.True(reloaded.HasCluster);
        Assert.Equal("Home", reloaded.ClusterName);
        Assert.Equal("AAAA-BBBB-CCCC-DDDD", reloaded.JoinKey);
        Assert.Equal(nodeId, Assert.Single(reloaded.Machines).NodeId);
    }

    [Fact]
    public void AMachineCannotJoinASecondClusterWithoutLeavingTheFirst()
    {
        // Belonging to two clusters would mean two sets of peers each believing they may inject
        // keystrokes here, with nothing reconciling them.
        ClusterMembership membership = CreateWithCluster();

        Assert.Throws<InvalidOperationException>(
            () => membership.CreateCluster("Other", "FEDCBA9876543210", "EEEE-FFFF-GGGG-HHHH", Thumbprint, "Desk"));
    }

    [Fact]
    public void LeavingClearsTheClusterButKeepsThisMachinesIdentity()
    {
        ClusterMembership membership = CreateWithCluster();
        Guid nodeId = membership.LocalNodeId;

        membership.LeaveCluster();

        Assert.False(membership.HasCluster);
        Assert.Empty(membership.Machines);
        Assert.Equal(string.Empty, membership.JoinKey);
        Assert.Equal(nodeId, Create().LocalNodeId);
    }

    [Fact]
    public void ThisMachineCannotRemoveItself()
    {
        // Removing yourself would leave a machine that is a member of a cluster it is tombstoned
        // in, which would keep trying to rejoin and keep being refused.
        ClusterMembership membership = CreateWithCluster();

        Assert.Throws<InvalidOperationException>(() => membership.RemoveMachine(membership.LocalNodeId));
    }

    [Fact]
    public void RemovingAMachineThatIsNotAMemberReportsSoRatherThanThrowing()
    {
        ClusterMembership membership = CreateWithCluster();

        Assert.False(membership.RemoveMachine(Guid.NewGuid()));
    }

    [Fact]
    public void RemovingAMachineLeavesATombstoneRatherThanAnAbsence()
    {
        // A peer that was powered off during the removal must not merge the machine — and its
        // certificate pin — back in when it returns.
        ClusterMembership membership = CreateWithCluster();
        Guid peer = EnrollPeer(membership, "Laptop");

        Assert.True(membership.RemoveMachine(peer));

        Assert.DoesNotContain(membership.Machines, entry => entry.NodeId == peer);
        Assert.Equal(MembershipState.Removed, membership.Find(peer)?.State);
    }

    [Fact]
    public void RemovingAMachineClearsAnyHotkeyPointingAtIt()
    {
        // Otherwise a familiar key would stay bound to a machine that is no longer a member, and
        // pressing it would silently do nothing while still looking configured.
        LocalSettingsStore settings = Settings();
        ClusterMembership membership = new(Store(), settings);
        membership.CreateCluster("Home", "0123456789ABCDEF", "AAAA-BBBB-CCCC-DDDD", Thumbprint, "Desk");

        Guid peer = EnrollPeer(membership, "Laptop");
        settings.Save(settings.Bind(settings.Load(), 3, peer));

        membership.RemoveMachine(peer);

        Assert.Null(LocalSettingsStore.TargetFor(settings.Load(), 3));
    }

    [Fact]
    public void MergingAPeersViewBringsInMachinesItKnowsAbout()
    {
        ClusterMembership membership = CreateWithCluster();

        var peerId = Guid.NewGuid();
        var theirRoster = new Roster(peerId);
        theirRoster.Enroll(peerId, Thumbprint, "Laptop");

        Assert.True(membership.Merge(theirRoster.Snapshot()));

        Assert.Contains(membership.Machines, entry => entry.NodeId == peerId && entry.DisplayName == "Laptop");
    }

    [Fact]
    public void MergingTheSameViewTwiceChangesNothingTheSecondTime()
    {
        // Anti-entropy will replay snapshots constantly, and each replay must not count as a
        // change or the roster would be rewritten to disk forever.
        ClusterMembership membership = CreateWithCluster();

        var peerId = Guid.NewGuid();
        var theirRoster = new Roster(peerId);
        theirRoster.Enroll(peerId, Thumbprint, "Laptop");
        RosterSnapshot snapshot = theirRoster.Snapshot();

        Assert.True(membership.Merge(snapshot));
        Assert.False(membership.Merge(snapshot));
    }

    [Fact]
    public void ThisMachineIsListedFirst()
    {
        // It is the one the person is sitting at, and the one they cannot remove, so it is not
        // left to sort in wherever its name happens to fall.
        ClusterMembership membership = CreateWithCluster();
        EnrollPeer(membership, "Aardvark");

        Assert.True(membership.Machines[0].NodeId == membership.LocalNodeId);
    }

    /// <summary>Adds a peer the way a completed enrollment eventually will.</summary>
    private static Guid EnrollPeer(ClusterMembership membership, string name)
    {
        var peerId = Guid.NewGuid();
        var theirRoster = new Roster(peerId);
        theirRoster.Enroll(peerId, Thumbprint, name);
        membership.Merge(theirRoster.Snapshot());
        return peerId;
    }

    public void Dispose()
    {
        try
        {
            if (Directory.Exists(_directory))
            {
                Directory.Delete(_directory, recursive: true);
            }
        }
        catch
        {
            // Best effort.
        }
    }
}
