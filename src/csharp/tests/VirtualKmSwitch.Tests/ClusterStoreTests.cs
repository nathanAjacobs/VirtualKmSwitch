using VirtualKmSwitch.Cluster;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class ClusterStoreTests : IDisposable
{
    private readonly string _directory =
        Path.Combine(Path.GetTempPath(), "VirtualKmSwitchTests", Guid.NewGuid().ToString("N"));

    private string RosterPath => Path.Combine(_directory, "roster.json");

    private ClusterStore CreateStore() => new(RosterPath);

    private static PersistedRoster SampleRoster(string clusterName = "home") => new()
    {
        ClusterId = "ABCD1234",
        ClusterName = clusterName,
        Clock = 7,
        Entries =
        [
            new RosterEntry
            {
                NodeId = Guid.Parse("11111111-1111-1111-1111-111111111111"),
                Thumbprint = "THUMB-A",
                DisplayName = "desktop",
                State = MembershipState.Enrolled,
                StateChangedAt = new LamportStamp(3, Guid.Empty),
            },
            new RosterEntry
            {
                NodeId = Guid.Parse("22222222-2222-2222-2222-222222222222"),
                Thumbprint = "THUMB-B",
                DisplayName = "laptop",
                State = MembershipState.Removed,
                StateChangedAt = new LamportStamp(7, Guid.Empty),
            },
        ],
    };

    [Fact]
    public void AMissingFileIsReportedAsAbsentRatherThanEmpty()
    {
        // The distinction matters: an empty roster looks exactly like every peer having left.
        LoadResult result = CreateStore().Load();

        Assert.Equal(LoadOutcome.Absent, result.Outcome);
        Assert.Null(result.Roster);
    }

    [Fact]
    public void RostersRoundTrip()
    {
        ClusterStore store = CreateStore();
        store.Save(SampleRoster());

        LoadResult result = store.Load();

        Assert.Equal(LoadOutcome.Loaded, result.Outcome);
        Assert.Equal("ABCD1234", result.Roster!.ClusterId);
        Assert.Equal(7UL, result.Roster.Clock);
        Assert.Equal(2, result.Roster.Entries.Count);

        // Tombstones must persist, or a restart would resurrect removed machines.
        Assert.Contains(result.Roster.Entries, entry => entry.State == MembershipState.Removed);
    }

    [Fact]
    public void SavingKeepsThePreviousCopyAsABackup()
    {
        ClusterStore store = CreateStore();
        store.Save(SampleRoster("first"));
        store.Save(SampleRoster("second"));

        Assert.True(File.Exists(store.BackupPath));
        Assert.Equal("second", store.Load().Roster!.ClusterName);
    }

    [Fact]
    public void TheVeryFirstSaveAlsoLeavesABackup()
    {
        // Otherwise a machine that has just joined a cluster holds exactly one copy of the
        // membership it would need to rejoin, until something happens to write a second time.
        // Losing it there means re-enrolling from another machine, which is precisely the task a
        // KVM cannot help with.
        ClusterStore store = CreateStore();
        store.Save(SampleRoster("first"));

        Assert.True(File.Exists(store.BackupPath));

        File.WriteAllText(store.FilePath, "{ corrupt");

        LoadResult result = store.Load();

        Assert.Equal(LoadOutcome.RecoveredFromBackup, result.Outcome);
        Assert.Equal("first", result.Roster!.ClusterName);
    }

    [Fact]
    public void ACorruptPrimaryFallsBackToTheBackup()
    {
        // The power-cut case. Without the fallback, membership becomes unreadable — and this is
        // the tool you would otherwise use to reach the other machines and fix it.
        ClusterStore store = CreateStore();
        store.Save(SampleRoster("original"));
        store.Save(SampleRoster("current"));

        File.WriteAllText(RosterPath, "{ this is not json");

        LoadResult result = store.Load();

        Assert.Equal(LoadOutcome.RecoveredFromBackup, result.Outcome);
        Assert.Equal("original", result.Roster!.ClusterName);
        Assert.NotNull(result.Error);
    }

    [Fact]
    public void AnEmptyPrimaryIsTreatedAsCorrupt()
    {
        // A zero-length file is what an interrupted write leaves behind, and it parses as nothing
        // rather than failing loudly.
        ClusterStore store = CreateStore();
        store.Save(SampleRoster("original"));
        store.Save(SampleRoster("current"));

        File.WriteAllBytes(RosterPath, []);

        Assert.Equal(LoadOutcome.RecoveredFromBackup, store.Load().Outcome);
    }

    [Fact]
    public void BothCopiesUnreadableIsReportedRatherThanSilentlyEmpty()
    {
        ClusterStore store = CreateStore();
        store.Save(SampleRoster("first"));
        store.Save(SampleRoster("second"));

        File.WriteAllText(RosterPath, "corrupt");
        File.WriteAllText(store.BackupPath, "also corrupt");

        LoadResult result = store.Load();

        Assert.Equal(LoadOutcome.Unrecoverable, result.Outcome);
        Assert.Null(result.Roster);
        Assert.NotNull(result.Error);
    }

    [Fact]
    public void UnknownFieldsSurviveARoundTrip()
    {
        // A peer on an older build must not strip settings it does not understand: it would write
        // the loss back and quietly remove them from the whole mesh.
        ClusterStore store = CreateStore();
        store.Save(SampleRoster());

        string json = File.ReadAllText(RosterPath);
        json = json.TrimEnd().TrimEnd('}') + ", \"futureSetting\": { \"nested\": 42 } }";
        File.WriteAllText(RosterPath, json);

        LoadResult loaded = store.Load();
        Assert.Equal(LoadOutcome.Loaded, loaded.Outcome);
        Assert.NotNull(loaded.Roster!.Extra);
        Assert.True(loaded.Roster.Extra!.ContainsKey("futureSetting"));

        store.Save(loaded.Roster);
        Assert.Contains("futureSetting", File.ReadAllText(RosterPath));
    }

    [Fact]
    public void NoTemporaryFileIsLeftBehind()
    {
        ClusterStore store = CreateStore();
        store.Save(SampleRoster());
        store.Save(SampleRoster());

        Assert.Empty(Directory.GetFiles(_directory, "*.tmp"));
    }

    [Fact]
    public void ARosterSurvivesTheFullRoundTripThroughTheCrdt()
    {
        var nodeA = Guid.NewGuid();
        var roster = new Roster(nodeA);
        roster.Enroll(nodeA, "THUMB-SELF", "self");
        roster.Enroll(Guid.NewGuid(), "THUMB-OTHER", "other");
        roster.Remove(roster.EnrolledEntries.Last().NodeId);

        ClusterStore store = CreateStore();
        RosterSnapshot snapshot = roster.Snapshot();

        store.Save(new PersistedRoster
        {
            ClusterId = "X",
            Clock = snapshot.Clock,
            Entries = snapshot.Entries,
        });

        PersistedRoster reloaded = store.Load().Roster!;
        var restored = new Roster(nodeA);
        restored.Load(new RosterSnapshot { Clock = reloaded.Clock, Entries = reloaded.Entries });

        Assert.Equal(roster.EnrolledCount, restored.EnrolledCount);
        Assert.Equal(roster.Clock, restored.Clock);
        Assert.Equal(roster.AllEntries.Count, restored.AllEntries.Count);
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
            // Temp cleanup is best effort.
        }
    }
}

public class ControlLeasesTests
{
    private static readonly Guid MachineA = Guid.NewGuid();
    private static readonly Guid PeerOne = Guid.NewGuid();
    private static readonly Guid PeerTwo = Guid.NewGuid();

    private static TimeSpan At(double seconds) => TimeSpan.FromSeconds(seconds);

    [Fact]
    public void TheFirstRequestWinsAndTheSecondIsRefused()
    {
        // Two people aiming at one machine would otherwise interleave keystrokes into a single
        // stream, unusable for both and mystifying to each.
        var leases = new ControlLeases();

        Assert.Equal(LeaseResult.Granted, leases.Request(At(0), MachineA, PeerOne));
        Assert.Equal(LeaseResult.HeldByAnother, leases.Request(At(0), MachineA, PeerTwo));
        Assert.Equal(PeerOne, leases.HolderOf(At(0), MachineA));
    }

    [Fact]
    public void TheHolderRenewsRatherThanBeingRefused()
    {
        var leases = new ControlLeases();
        leases.Request(At(0), MachineA, PeerOne);

        Assert.Equal(LeaseResult.Renewed, leases.Request(At(1), MachineA, PeerOne));
        Assert.Equal(PeerOne, leases.HolderOf(At(3.5), MachineA));
    }

    [Fact]
    public void ALeaseExpiresSoADeadPeerDoesNotHoldAMachineForever()
    {
        // The usual ending is a machine sleeping or losing the network, not a polite release.
        var leases = new ControlLeases();
        leases.Request(At(0), MachineA, PeerOne);

        Assert.Null(leases.HolderOf(At(10), MachineA));
        Assert.Equal(LeaseResult.Granted, leases.Request(At(10), MachineA, PeerTwo));
    }

    [Fact]
    public void ReleasingFreesTheTargetImmediately()
    {
        var leases = new ControlLeases();
        leases.Request(At(0), MachineA, PeerOne);

        Assert.True(leases.Release(MachineA, PeerOne));
        Assert.Equal(LeaseResult.Granted, leases.Request(At(0), MachineA, PeerTwo));
    }

    [Fact]
    public void OnlyTheHolderCanRelease()
    {
        var leases = new ControlLeases();
        leases.Request(At(0), MachineA, PeerOne);

        Assert.False(leases.Release(MachineA, PeerTwo));
        Assert.Equal(PeerOne, leases.HolderOf(At(0), MachineA));
    }

    [Fact]
    public void DisconnectingReleasesEverythingThatPeerHeld()
    {
        var leases = new ControlLeases();
        var machineB = Guid.NewGuid();

        leases.Request(At(0), MachineA, PeerOne);
        leases.Request(At(0), machineB, PeerOne);

        Assert.Equal(2, leases.ReleaseAllHeldBy(PeerOne));
        Assert.Null(leases.HolderOf(At(0), MachineA));
        Assert.Null(leases.HolderOf(At(0), machineB));
    }

    [Fact]
    public void ANewHostRebuildsLeasesFromWhatPeersReport()
    {
        // Leases describe who is typing right now, so carrying them across a host change would
        // hand out stale claims. They are rebuilt, never persisted.
        var leases = new ControlLeases();
        leases.Rebuild([new ControlLease(MachineA, PeerTwo, At(5))]);

        Assert.Equal(PeerTwo, leases.HolderOf(At(1), MachineA));
        Assert.Single(leases.Active(At(1)));
        Assert.Empty(leases.Active(At(6)));
    }
}
