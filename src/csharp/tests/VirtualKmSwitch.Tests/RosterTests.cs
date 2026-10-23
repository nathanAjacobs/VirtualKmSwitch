using VirtualKmSwitch.Cluster;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class RosterTests
{
    private static readonly Guid NodeA = new("11111111-1111-1111-1111-111111111111");
    private static readonly Guid NodeB = new("22222222-2222-2222-2222-222222222222");
    private static readonly Guid NodeC = new("33333333-3333-3333-3333-333333333333");

    private static string Fingerprint(Guid nodeId) => nodeId.ToString("N")[..16].ToUpperInvariant();

    [Fact]
    public void EnrollingAddsAMember()
    {
        var roster = new Roster(NodeA);
        roster.Enroll(NodeB, Fingerprint(NodeB), "Bob");

        Assert.True(roster.IsEnrolled(NodeB));
        Assert.Equal(1, roster.EnrolledCount);
        Assert.Equal("Bob", roster.Find(NodeB)!.DisplayName);
    }

    [Fact]
    public void RemovingLeavesATombstoneRatherThanDeleting()
    {
        // The tombstone is what makes the removal survive a merge with a peer that never saw it.
        var roster = new Roster(NodeA);
        roster.Enroll(NodeB, Fingerprint(NodeB));
        roster.Remove(NodeB);

        Assert.False(roster.IsEnrolled(NodeB));
        Assert.Equal(0, roster.EnrolledCount);
        Assert.Single(roster.AllEntries);
        Assert.Equal(MembershipState.Removed, roster.Find(NodeB)!.State);
    }

    [Fact]
    public void APeerWithAStaleCopyCannotResurrectARemovedMachine()
    {
        // The scenario the tombstone exists for: a machine powered off during a removal, merging
        // its older view back in when it returns.
        var authoritative = new Roster(NodeA);
        authoritative.Enroll(NodeB, Fingerprint(NodeB));

        var stale = new Roster(NodeC);
        stale.Merge(authoritative.Snapshot());

        authoritative.Remove(NodeB);
        authoritative.Merge(stale.Snapshot());

        Assert.False(authoritative.IsEnrolled(NodeB));

        // And the stale peer learns of the removal when it merges the other way.
        stale.Merge(authoritative.Snapshot());
        Assert.False(stale.IsEnrolled(NodeB));
    }

    [Fact]
    public void ReEnrollingAfterRemovalWins()
    {
        // Deliberately re-adding a machine has to beat the tombstone, or a removal would be
        // permanent and the machine could never rejoin.
        var roster = new Roster(NodeA);
        roster.Enroll(NodeB, Fingerprint(NodeB));
        roster.Remove(NodeB);
        roster.Enroll(NodeB, "NEWTHUMBPRINT");

        Assert.True(roster.IsEnrolled(NodeB));
        Assert.Equal("NEWTHUMBPRINT", roster.Find(NodeB)!.Thumbprint);
    }

    [Fact]
    public void MergingIsIdempotent()
    {
        var left = new Roster(NodeA);
        left.Enroll(NodeB, Fingerprint(NodeB), "Bob");

        var right = new Roster(NodeC);
        right.Merge(left.Snapshot());

        RosterSnapshot before = right.Snapshot();
        right.Merge(left.Snapshot());
        right.Merge(left.Snapshot());

        Assert.Equal(before.Entries.Count, right.Snapshot().Entries.Count);
        Assert.True(right.IsEnrolled(NodeB));
    }

    [Fact]
    public void MergingIsCommutative()
    {
        var alice = new Roster(NodeA);
        alice.Enroll(NodeB, Fingerprint(NodeB), "Bob");

        var carol = new Roster(NodeC);
        carol.Enroll(NodeC, Fingerprint(NodeC), "Carol");

        var aliceThenCarol = new Roster(Guid.NewGuid());
        aliceThenCarol.Merge(alice.Snapshot());
        aliceThenCarol.Merge(carol.Snapshot());

        var carolThenAlice = new Roster(Guid.NewGuid());
        carolThenAlice.Merge(carol.Snapshot());
        carolThenAlice.Merge(alice.Snapshot());

        AssertSameMembership(aliceThenCarol, carolThenAlice);
    }

    [Fact]
    public void ConcurrentEnrolAndRemoveResolveTheSameWayOnBothSides()
    {
        // A partition: one side removes a machine while the other re-enrolls it. Both must land on
        // the same answer, whichever order they exchange rosters in.
        var shared = new Roster(NodeA);
        shared.Enroll(NodeB, Fingerprint(NodeB));

        var remover = new Roster(NodeA);
        remover.Load(shared.Snapshot());
        remover.Remove(NodeB);

        var reEnroller = new Roster(NodeC);
        reEnroller.Load(shared.Snapshot());
        reEnroller.Enroll(NodeB, "REENROLLED");

        var wayOne = new Roster(Guid.NewGuid());
        wayOne.Merge(remover.Snapshot());
        wayOne.Merge(reEnroller.Snapshot());

        var wayTwo = new Roster(Guid.NewGuid());
        wayTwo.Merge(reEnroller.Snapshot());
        wayTwo.Merge(remover.Snapshot());

        Assert.Equal(wayOne.IsEnrolled(NodeB), wayTwo.IsEnrolled(NodeB));
        Assert.Equal(wayOne.Find(NodeB)!.Thumbprint, wayTwo.Find(NodeB)!.Thumbprint);
    }

    [Fact]
    public void AMachineOwnsItsOwnName()
    {
        // Membership and name have separate stamps, so a remover's stale copy of the name cannot
        // overwrite what the machine calls itself.
        var machine = new Roster(NodeB);
        machine.Enroll(NodeB, Fingerprint(NodeB), "old-name");
        machine.SetDisplayName(NodeB, "renamed-by-owner");

        var other = new Roster(NodeA);
        other.Enroll(NodeB, Fingerprint(NodeB), "old-name");
        other.Merge(machine.Snapshot());

        Assert.Equal("renamed-by-owner", other.Find(NodeB)!.DisplayName);
    }

    [Fact]
    public void MergingAdvancesTheClockPastEverythingObserved()
    {
        // A node returning from a long absence must not keep issuing stamps that lose to changes
        // it has just learned about.
        var busy = new Roster(NodeA);
        for (int i = 0; i < 20; i++)
        {
            busy.Enroll(Guid.NewGuid(), $"THUMB{i}");
        }

        var quiet = new Roster(NodeC);
        Assert.Equal(0UL, quiet.Clock);

        quiet.Merge(busy.Snapshot());
        Assert.True(quiet.Clock >= busy.Clock);

        // A change made now outranks everything it merged.
        quiet.Enroll(NodeB, Fingerprint(NodeB));
        Assert.True(quiet.Find(NodeB)!.StateChangedAt.Counter > busy.Clock);
    }

    [Fact]
    public void EqualStampsAreBrokenDeterministically()
    {
        // Two nodes acting at the same logical time must still converge; without a tie-break each
        // would keep its own and the meshes would never agree.
        var lower = new LamportStamp(5, NodeA);
        var higher = new LamportStamp(5, NodeC);

        Assert.NotEqual(0, lower.CompareTo(higher));
        Assert.Equal(-Math.Sign(lower.CompareTo(higher)), Math.Sign(higher.CompareTo(lower)));
    }

    /// <summary>
    /// The property that matters: any set of changes, delivered in any order, converges.
    ///
    /// Enumerated cases only cover the orderings someone thought of. This runs randomised
    /// histories against three replicas exchanging in different sequences, which is where a
    /// non-commutative merge actually shows up.
    /// </summary>
    [Theory]
    [InlineData(1)]
    [InlineData(7)]
    [InlineData(42)]
    [InlineData(1337)]
    [InlineData(90210)]
    public void AnyDeliveryOrderConverges(int seed)
    {
        var random = new Random(seed);
        Guid[] nodes = [NodeA, NodeB, NodeC];
        Guid[] machines = [.. Enumerable.Range(0, 6).Select(_ => Guid.NewGuid())];

        Roster[] replicas = [.. nodes.Select(node => new Roster(node))];

        // Each replica applies its own random history, entirely independently.
        for (int round = 0; round < 60; round++)
        {
            Roster replica = replicas[random.Next(replicas.Length)];
            Guid machine = machines[random.Next(machines.Length)];

            switch (random.Next(3))
            {
                case 0:
                    replica.Enroll(machine, $"THUMB-{random.Next(1000)}");
                    break;

                case 1:
                    replica.Remove(machine);
                    break;

                default:
                    replica.SetDisplayName(machine, $"name-{random.Next(100)}");
                    break;
            }
        }

        RosterSnapshot[] snapshots = [.. replicas.Select(replica => replica.Snapshot())];

        // Exchange everything with everyone, repeatedly and in shuffled order, which is how a real
        // mesh gossips.
        for (int pass = 0; pass < 4; pass++)
        {
            foreach (Roster replica in replicas.OrderBy(_ => random.Next()))
            {
                foreach (RosterSnapshot snapshot in snapshots.OrderBy(_ => random.Next()))
                {
                    replica.Merge(snapshot);
                }
            }

            snapshots = [.. replicas.Select(replica => replica.Snapshot())];
        }

        for (int i = 1; i < replicas.Length; i++)
        {
            AssertSameMembership(replicas[0], replicas[i]);
        }
    }

    [Fact]
    public void SnapshotsRoundTripThroughLoad()
    {
        var original = new Roster(NodeA);
        original.Enroll(NodeB, Fingerprint(NodeB), "Bob");
        original.Enroll(NodeC, Fingerprint(NodeC), "Carol");
        original.Remove(NodeC);

        var restored = new Roster(NodeA);
        restored.Load(original.Snapshot());

        AssertSameMembership(original, restored);
        Assert.Equal(original.Clock, restored.Clock);
    }

    private static void AssertSameMembership(Roster left, Roster right)
    {
        Dictionary<Guid, RosterEntry> leftEntries = left.AllEntries.ToDictionary(entry => entry.NodeId);
        Dictionary<Guid, RosterEntry> rightEntries = right.AllEntries.ToDictionary(entry => entry.NodeId);

        Assert.Equal(leftEntries.Keys.OrderBy(id => id), rightEntries.Keys.OrderBy(id => id));

        foreach ((Guid nodeId, RosterEntry leftEntry) in leftEntries)
        {
            RosterEntry rightEntry = rightEntries[nodeId];

            Assert.Equal(leftEntry.State, rightEntry.State);
            Assert.Equal(leftEntry.Thumbprint, rightEntry.Thumbprint);
            Assert.Equal(leftEntry.DisplayName, rightEntry.DisplayName);
        }
    }
}
