using VirtualKmSwitch.Cluster;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// The election matrix, driven by an injected clock so every timing case runs instantly and
/// deterministically rather than by sleeping.
/// </summary>
public class HostElectionTests
{
    private static readonly Guid LowId = new("11111111-1111-1111-1111-111111111111");
    private static readonly Guid MidId = new("55555555-5555-5555-5555-555555555555");
    private static readonly Guid HighId = new("99999999-9999-9999-9999-999999999999");

    private static readonly ElectionTimings Timings = new();

    private static HostElection Node(Guid nodeId, int priority = 0, ulong term = 0) =>
        new(new NodeRank(priority, nodeId), term, Timings);

    private static TimeSpan At(double milliseconds) => TimeSpan.FromMilliseconds(milliseconds);

    /// <summary>Runs the clock forward in small steps, collecting what the node broadcasts.</summary>
    private static List<ElectionEffect> Advance(HostElection election, double fromMs, double toMs, double stepMs = 50)
    {
        var effects = new List<ElectionEffect>();

        for (double now = fromMs; now <= toMs; now += stepMs)
        {
            ElectionEffect effect = election.Tick(At(now));
            if (effect != ElectionEffect.None)
            {
                effects.Add(effect);
            }
        }

        return effects;
    }

    [Fact]
    public void ALoneNodeBecomesHostAfterTheDiscoveryWindow()
    {
        // A cluster of one -- the create-cluster case -- must reach a host without waiting on
        // anyone else to answer.
        HostElection node = Node(LowId);

        Assert.Equal(HostRole.Follower, node.Role);

        Advance(node, 0, 500);
        Assert.Equal(HostRole.Follower, node.Role);

        Advance(node, 500, 2500);
        Assert.Equal(HostRole.Host, node.Role);
        Assert.Equal(LowId, node.CurrentHost);
        Assert.True(node.Term > 0);
    }

    /// <summary>
    /// Advances the clock while a live host keeps heartbeating, which is what actually suppresses
    /// an election. A single heartbeat does not: after the timeout the host is presumed gone, and
    /// correctly so.
    /// </summary>
    private static void AdvanceWithLiveHost(
        HostElection election, double fromMs, double toMs, Guid hostId, HostClaim claim, double stepMs = 50)
    {
        double lastHeartbeat = fromMs;

        for (double now = fromMs; now <= toMs; now += stepMs)
        {
            if (now - lastHeartbeat >= Timings.HeartbeatInterval.TotalMilliseconds)
            {
                election.OnHeartbeat(At(now), hostId, claim);
                lastHeartbeat = now;
            }

            election.Tick(At(now));
        }
    }

    [Fact]
    public void ALiveHostSuppressesElectionsIndefinitely()
    {
        HostElection node = Node(LowId);
        var claim = new HostClaim(5, new NodeRank(0, HighId));

        node.OnHeartbeat(At(100), HighId, claim);
        AdvanceWithLiveHost(node, 100, 5000, HighId, claim);

        Assert.Equal(HostRole.Follower, node.Role);
        Assert.Equal(HighId, node.CurrentHost);
        Assert.Equal(5UL, node.Term);
    }

    [Fact]
    public void ASingleHeartbeatOnlyDelaysAnElectionByTheTimeout()
    {
        // The converse, and the reason the test above must keep heartbeating: one heartbeat proves
        // a host was alive at that instant, not that it still is.
        HostElection node = Node(LowId);
        node.OnHeartbeat(At(100), HighId, new HostClaim(5, new NodeRank(0, HighId)));

        Advance(node, 100, 700);
        Assert.Equal(HostRole.Follower, node.Role);

        Advance(node, 700, 2000);
        Assert.Equal(HostRole.Host, node.Role);
    }

    [Fact]
    public void AHostThatGoesSilentIsReplaced()
    {
        HostElection node = Node(LowId);
        node.OnHeartbeat(At(100), HighId, new HostClaim(3, new NodeRank(0, HighId)));

        // Still within the timeout: the host is presumed alive.
        Advance(node, 100, 700);
        Assert.Equal(HostRole.Follower, node.Role);

        Advance(node, 700, 2000);
        Assert.Equal(HostRole.Host, node.Role);
        Assert.True(node.Term > 3);
    }

    [Fact]
    public void ABetterRankedNodeJoiningDoesNotDisturbAHealthyHost()
    {
        // The rule that keeps the role stable as a cluster wakes up. Without it, every machine
        // powering on in the morning would trigger a handover.
        HostElection host = Node(HighId);
        Advance(host, 0, 2500);
        Assert.Equal(HostRole.Host, host.Role);

        ulong termWhileHosting = host.Term;

        // A better-ranked latecomer starts up while the incumbent keeps heartbeating.
        HostElection latecomer = Node(LowId, priority: 100);
        var incumbentClaim = new HostClaim(termWhileHosting, host.Rank);
        AdvanceWithLiveHost(latecomer, 3000, 6000, HighId, incumbentClaim);

        Assert.Equal(HostRole.Follower, latecomer.Role);
        Assert.Equal(HighId, latecomer.CurrentHost);

        // And it never campaigns: a follower emits neither a claim nor a heartbeat, which is what
        // keeps the incumbent from ever hearing anything that could dethrone it. Rank at an equal
        // term only ever arises between two hosts, which is the split-brain case.
        List<ElectionEffect> emitted = [];
        double lastHeartbeat = 6000;

        for (double now = 6000; now <= 9000; now += 50)
        {
            if (now - lastHeartbeat >= Timings.HeartbeatInterval.TotalMilliseconds)
            {
                latecomer.OnHeartbeat(At(now), HighId, incumbentClaim);
                lastHeartbeat = now;
            }

            ElectionEffect effect = latecomer.Tick(At(now));
            if (effect != ElectionEffect.None)
            {
                emitted.Add(effect);
            }
        }

        Assert.Empty(emitted);
        Assert.Equal(HostRole.Follower, latecomer.Role);
    }

    [Fact]
    public void OnlyAHostEverEmitsAHeartbeat()
    {
        // The invariant the incumbent-stability rule rests on. If a follower or candidate could
        // heartbeat, a better-ranked one would dethrone a healthy host at an equal term.
        HostElection node = Node(LowId);

        List<ElectionEffect> whileFollower = Advance(node, 0, 1400);
        Assert.DoesNotContain(ElectionEffect.BroadcastHeartbeat, whileFollower);

        List<ElectionEffect> throughCandidacy = Advance(node, 1400, 1900);
        Assert.Contains(ElectionEffect.BroadcastClaim, throughCandidacy);
        Assert.DoesNotContain(ElectionEffect.BroadcastHeartbeat, throughCandidacy);

        Advance(node, 1900, 2500);
        Assert.Equal(HostRole.Host, node.Role);

        List<ElectionEffect> asHost = Advance(node, 2500, 3200);
        Assert.Contains(ElectionEffect.BroadcastHeartbeat, asHost);
    }

    [Fact]
    public void RankDecidesOnlyWhenTermsAreEqual()
    {
        var lowRank = new HostClaim(5, new NodeRank(0, HighId));
        var highRank = new HostClaim(5, new NodeRank(10, LowId));

        Assert.True(highRank.Beats(lowRank));
        Assert.False(lowRank.Beats(highRank));

        // A higher term wins regardless of rank -- which is what makes deliberate takeover work.
        var laterButLowerRank = new HostClaim(6, new NodeRank(0, HighId));
        Assert.True(laterButLowerRank.Beats(highRank));
    }

    [Fact]
    public void ManualTakeoverBeatsABetterRankedIncumbent()
    {
        // Someone sitting at a machine asking for control should get it, not be told another
        // machine outranks them.
        HostElection incumbent = Node(HighId, priority: 100);
        Advance(incumbent, 0, 2500);
        Assert.Equal(HostRole.Host, incumbent.Role);

        HostElection challenger = Node(LowId);
        challenger.OnHeartbeat(At(3000), HighId, new HostClaim(incumbent.Term, incumbent.Rank));

        Assert.Equal(ElectionEffect.BroadcastClaim, challenger.ClaimHost(At(3100)));
        Assert.True(challenger.Term > incumbent.Term);

        Advance(challenger, 3100, 4000);
        Assert.Equal(HostRole.Host, challenger.Role);

        // The incumbent stands down on seeing the higher term.
        incumbent.OnHeartbeat(At(4100), LowId, new HostClaim(challenger.Term, challenger.Rank));
        Assert.Equal(HostRole.Follower, incumbent.Role);
        Assert.Equal(LowId, incumbent.CurrentHost);
    }

    [Fact]
    public void SplitBrainResolvesToExactlyOneHost()
    {
        // Two partitions each elect a host; when they heal, the strict total order must leave
        // precisely one standing.
        HostElection left = Node(LowId, priority: 5);
        HostElection right = Node(HighId, priority: 5);

        Advance(left, 0, 2500);
        Advance(right, 0, 2500);
        Assert.Equal(HostRole.Host, left.Role);
        Assert.Equal(HostRole.Host, right.Role);

        // The partition heals and each sees the other's heartbeat.
        left.OnHeartbeat(At(3000), HighId, new HostClaim(right.Term, right.Rank));
        right.OnHeartbeat(At(3000), LowId, new HostClaim(left.Term, left.Rank));

        Assert.True(
            left.IsHost ^ right.IsHost,
            $"exactly one host expected, got left={left.Role} right={right.Role}");
    }

    [Fact]
    public void AYieldingHostDoesNotImmediatelyWinTheElectionItCaused()
    {
        HostElection host = Node(LowId);
        Advance(host, 0, 2500);
        Assert.Equal(HostRole.Host, host.Role);

        Assert.Equal(ElectionEffect.BroadcastYield, host.Yield(At(3000)));
        Assert.Equal(HostRole.Follower, host.Role);

        // Held out of the running while another node takes over.
        Advance(host, 3000, 5000);
        Assert.Equal(HostRole.Follower, host.Role);

        // Eligible again once the cooldown expires and nobody else has taken the role.
        Advance(host, 5000, 8000);
        Assert.Equal(HostRole.Host, host.Role);
    }

    [Fact]
    public void AYieldFromTheCurrentHostStartsAnElectionWithoutWaitingOut()
    {
        HostElection follower = Node(LowId);
        follower.OnHeartbeat(At(100), HighId, new HostClaim(3, new NodeRank(0, HighId)));

        follower.OnYield(At(200), HighId);

        // The full heartbeat timeout is not waited out, because the host said it was leaving.
        Advance(follower, 200, 900);
        Assert.Equal(HostRole.Host, follower.Role);
    }

    [Fact]
    public void ACandidateStandsDownForABetterClaim()
    {
        HostElection candidate = Node(LowId);
        Advance(candidate, 0, 1600);
        Assert.Equal(HostRole.Candidate, candidate.Role);

        candidate.OnClaim(At(1650), HighId, new HostClaim(candidate.Term + 5, new NodeRank(0, HighId)));

        Assert.Equal(HostRole.Follower, candidate.Role);
        Assert.Equal(HighId, candidate.CurrentHost);

        Advance(candidate, 1650, 2000);
        Assert.Equal(HostRole.Follower, candidate.Role);
    }

    [Fact]
    public void ACandidateIgnoresAnInferiorClaim()
    {
        HostElection candidate = Node(HighId, priority: 10);
        Advance(candidate, 0, 1600);
        Assert.Equal(HostRole.Candidate, candidate.Role);

        candidate.OnClaim(At(1650), LowId, new HostClaim(candidate.Term, new NodeRank(0, LowId)));

        Assert.Equal(HostRole.Candidate, candidate.Role);

        Advance(candidate, 1650, 2500);
        Assert.Equal(HostRole.Host, candidate.Role);
    }

    [Fact]
    public void AHostHeartbeatsAtTheConfiguredInterval()
    {
        HostElection host = Node(LowId);
        Advance(host, 0, 2500);
        Assert.Equal(HostRole.Host, host.Role);

        List<ElectionEffect> effects = Advance(host, 2500, 3500, stepMs: 50);

        Assert.All(effects, effect => Assert.Equal(ElectionEffect.BroadcastHeartbeat, effect));

        // Roughly one per interval over a second, not one per tick.
        Assert.InRange(effects.Count, 3, 6);
    }

    /// <summary>
    /// The determinism the plan asks for: the same set of nodes must elect the same host every
    /// time, or a cluster would settle differently after each restart.
    /// </summary>
    [Theory]
    [InlineData(1)]
    [InlineData(99)]
    [InlineData(12345)]
    public void TheSameNodeSetAlwaysElectsTheSameHost(int seed)
    {
        var random = new Random(seed);
        (Guid Id, int Priority)[] candidates =
        [
            (LowId, random.Next(0, 3)),
            (MidId, random.Next(0, 3)),
            (HighId, random.Next(0, 3)),
        ];

        Guid? firstWinner = null;

        // Repeat the contest with the nodes campaigning in different orders each round.
        for (int attempt = 0; attempt < 8; attempt++)
        {
            HostElection[] nodes =
                [.. candidates.OrderBy(_ => random.Next()).Select(c => Node(c.Id, c.Priority))];

            foreach (HostElection node in nodes)
            {
                Advance(node, 0, 1600);
            }

            // Everyone hears everyone else's claim.
            foreach (HostElection node in nodes)
            {
                foreach (HostElection other in nodes.Where(n => !ReferenceEquals(n, node)))
                {
                    node.OnClaim(At(1650), other.Rank.NodeId, new HostClaim(other.Term, other.Rank));
                }
            }

            foreach (HostElection node in nodes)
            {
                Advance(node, 1650, 2500);
            }

            HostElection[] hosts = [.. nodes.Where(node => node.IsHost)];
            Assert.Single(hosts);

            firstWinner ??= hosts[0].Rank.NodeId;
            Assert.Equal(firstWinner, hosts[0].Rank.NodeId);
        }
    }
}
