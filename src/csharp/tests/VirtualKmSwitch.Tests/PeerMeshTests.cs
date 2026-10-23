using System.Diagnostics;
using System.Security.Cryptography.X509Certificates;
using VirtualKmSwitch.Core;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// Two complete meshes on loopback: real listeners, real dialling, real mutual TLS, real pinning.
/// </summary>
public class PeerMeshTests
{
    /// <summary>Ephemeral ports per test so parallel runs cannot collide.</summary>
    private static int NextPort() => Random.Shared.Next(45_000, 60_000);

    private sealed class TwoNodes : IAsyncDisposable
    {
        public TwoNodes()
        {
            AliceCertificate = NodeCertificate.Create("CN=MeshAlice", StoreLocation.CurrentUser);
            BobCertificate = NodeCertificate.Create("CN=MeshBob", StoreLocation.CurrentUser);

            // Ordered so Alice is always the dialler, matching the lower-id-dials rule.
            Guid first = Guid.NewGuid();
            Guid second = Guid.NewGuid();
            (AliceId, BobId) = first.CompareTo(second) < 0 ? (first, second) : (second, first);

            AlicePins.Pin(NodeCertificate.ThumbprintOf(BobCertificate), BobId);
            BobPins.Pin(NodeCertificate.ThumbprintOf(AliceCertificate), AliceId);

            AlicePort = NextPort();
            BobPort = AlicePort + 1;

            Alice = new PeerMesh(AliceId, AliceCertificate, AlicePins, AlicePort);
            Bob = new PeerMesh(BobId, BobCertificate, BobPins, BobPort);
        }

        public X509Certificate2 AliceCertificate { get; }

        public X509Certificate2 BobCertificate { get; }

        public Guid AliceId { get; }

        public Guid BobId { get; }

        public PinStore AlicePins { get; } = new();

        public PinStore BobPins { get; } = new();

        public int AlicePort { get; }

        public int BobPort { get; }

        public PeerMesh Alice { get; }

        public PeerMesh Bob { get; }

        public async Task<bool> ConnectAsync()
        {
            Alice.Start();
            Bob.Start();

            return await Alice.EnsureConnectedAsync(BobId, "127.0.0.1", BobPort);
        }

        public async ValueTask DisposeAsync()
        {
            await Alice.DisposeAsync();
            await Bob.DisposeAsync();
            AliceCertificate.Dispose();
            BobCertificate.Dispose();
        }
    }

    private static async Task WaitForAsync(Func<bool> condition, TimeSpan timeout)
    {
        var deadline = Stopwatch.StartNew();
        while (deadline.Elapsed < timeout)
        {
            if (condition())
            {
                return;
            }

            await Task.Delay(20);
        }
    }

    [Fact]
    public void DiallingIsDecidedByNodeOrderSoBothSidesAgree()
    {
        // Both peers see each other simultaneously. Without a total order they would dial at once
        // and build two links where one is wanted; each side evaluates this identically.
        var low = new Guid("00000000-0000-0000-0000-000000000001");
        var high = new Guid("ffffffff-ffff-ffff-ffff-ffffffffffff");

        using X509Certificate2 certificate = NodeCertificate.Create("CN=Order", StoreLocation.CurrentUser);

        var lowMesh = new PeerMesh(low, certificate, new PinStore());
        var highMesh = new PeerMesh(high, certificate, new PinStore());

        Assert.True(lowMesh.ShouldDial(high));
        Assert.False(highMesh.ShouldDial(low));
    }

    [Fact]
    public async Task PeersConnectAndBothSidesSeeTheLink()
    {
        await using var nodes = new TwoNodes();

        Assert.True(await nodes.ConnectAsync());
        await WaitForAsync(() => nodes.Bob.ConnectionCount > 0, TimeSpan.FromSeconds(10));

        Assert.Contains(nodes.BobId, nodes.Alice.ConnectedPeers);
        Assert.Contains(nodes.AliceId, nodes.Bob.ConnectedPeers);
    }

    [Fact]
    public async Task InputFlowsBetweenMeshes()
    {
        await using var nodes = new TwoNodes();
        Assert.True(await nodes.ConnectAsync());

        var received = new List<InputEvent>();
        var done = new TaskCompletionSource();

        nodes.Bob.InputReceived += (_, events, _) =>
        {
            foreach (ref readonly InputEvent inputEvent in events)
            {
                received.Add(inputEvent);
            }

            done.TrySetResult();
        };

        await WaitForAsync(() => nodes.Bob.ConnectionCount > 0, TimeSpan.FromSeconds(10));

        await nodes.Alice.SendInputAsync(
            nodes.BobId, 42, new[] { InputEvent.Key(42, 0x1E, InputEventFlags.None) });

        await done.Task.WaitAsync(TimeSpan.FromSeconds(10));

        Assert.Single(received);
        Assert.Equal(0x1E, received[0].Code);
    }

    [Fact]
    public async Task SendingToAnUnknownPeerIsIgnoredRatherThanThrowing()
    {
        // Peers disappear constantly — sleep, reboot, network change. A send racing a disconnect
        // is normal traffic, not an error worth propagating into the input path.
        await using var nodes = new TwoNodes();
        nodes.Alice.Start();

        await nodes.Alice.SendInputAsync(Guid.NewGuid(), 1, new[] { InputEvent.MouseMove(1, 1, 1) });
    }

    [Fact]
    public async Task AnUnpinnedPeerCannotJoinTheMesh()
    {
        await using var nodes = new TwoNodes();

        // Bob forgets Alice, as removing her from the mesh would do.
        nodes.BobPins.Clear();

        nodes.Alice.Start();
        nodes.Bob.Start();

        Assert.False(await nodes.Alice.EnsureConnectedAsync(nodes.BobId, "127.0.0.1", nodes.BobPort));
        Assert.Empty(nodes.Alice.ConnectedPeers);
    }

    [Fact]
    public async Task AnEightKilohertzStreamStaysInsideTheLatencyBudgetAndKeepsItsOrder()
    {
        // The plan budgets p99 added latency at 2 ms and calls for a synthetic 8 kHz load through
        // the full pipeline. On loopback the wire costs nothing, so what is measured is the queue,
        // the coalescer, the codec and TLS -- and it should sit far below the budget, leaving the
        // rest of it for a real network.
        //
        // Pacing matters as much as the count. An unpaced loop produces events faster than any
        // mouse in existence, the queue backs up, and the resulting figure measures the test
        // harness rather than the transport. Measured that way the same code reports 6 ms.
        await using var nodes = new TwoNodes();
        Assert.True(await nodes.ConnectAsync());
        await WaitForAsync(() => nodes.Bob.ConnectionCount > 0, TimeSpan.FromSeconds(10));

        const int events = 2_000;
        const long periodMicroseconds = 125;    // 8 kHz

        var received = new List<ushort>(events);
        var done = new TaskCompletionSource();

        // Measured a second way, straight from the batch timestamp, with no clock offset applied.
        // On loopback both ends read the same clock so the two must agree, and holding them to
        // that caught a real bug: a cold first heartbeat biased every reported figure by 2.3 ms
        // while this direct measurement stayed at 98 us.
        var direct = new List<long>(events);

        nodes.Bob.InputReceived += (_, batch, capture) =>
        {
            direct.Add(MonotonicClock.Microseconds - capture);

            foreach (InputEvent inputEvent in batch)
            {
                received.Add(inputEvent.Code);
            }

            if (received.Count >= events)
            {
                done.TrySetResult();
            }
        };

        // Key events, not motion: motion is deliberately allowed to merge, so a test that counted
        // it would be asserting on how the scheduler happened to interleave.
        long next = MonotonicClock.Microseconds;
        for (int i = 0; i < events; i++)
        {
            while (MonotonicClock.Microseconds < next)
            {
                Thread.SpinWait(10);
            }

            next += periodMicroseconds;
            Assert.True(nodes.Alice.TrySend(
                nodes.BobId, InputEvent.Key(Stopwatch.GetTimestamp(), (ushort)i, InputEventFlags.None)));
        }

        await done.Task.WaitAsync(TimeSpan.FromSeconds(30));

        PeerConnection connection = nodes.Bob.Connection(nodes.AliceId)!;
        PeerSendPump pump = nodes.Alice.Pump(nodes.BobId)!;
        long p99 = connection.Latency.Percentile(0.99);

        Assert.Equal(events, received.Count);

        // Nothing reordered. A key-up that overtook its key-down would leave the key held, which
        // is the failure the whole ordered-stream design exists to prevent.
        Assert.Equal(Enumerable.Range(0, events).Select(i => (ushort)i), received);

        Assert.Equal(0, pump.MovesDropped);
        Assert.True(
            p99 < 2_000,
            $"p99 was {p99}us, over the 2ms budget. Summary: {connection.Latency.Summary()}");

        // The two measurements must not disagree by more than the histogram's own bucket width
        // plus the work between them. A gap far larger than that means the clock estimate is
        // biased, and the reported figures are then confidently wrong rather than merely noisy.
        long directMedian = direct.Order().ElementAt(direct.Count / 2);
        Assert.InRange(connection.Latency.Percentile(0.50), 0, directMedian + 500);

        Assert.True(
            connection.Clock.HasSample,
            "the link reported latency without ever having synchronised its clock");
    }

    [Fact]
    public async Task ClockSynchronisationConvergesWithoutBeingAskedTo()
    {
        await using var nodes = new TwoNodes();
        Assert.True(await nodes.ConnectAsync());
        await WaitForAsync(() => nodes.Bob.ConnectionCount > 0, TimeSpan.FromSeconds(10));

        PeerConnection fromAlice = nodes.Bob.Connection(nodes.AliceId)!;
        PeerConnection fromBob = nodes.Alice.Connection(nodes.BobId)!;

        await WaitForAsync(
            () => fromAlice.Clock.HasSample && fromBob.Clock.HasSample, TimeSpan.FromSeconds(10));

        // Both ends of a loopback link read the same clock, so the true offset is zero. Anything
        // else here would be the estimator inventing a difference, and on two real machines that
        // invention is indistinguishable from latency.
        Assert.InRange(fromAlice.Clock.OffsetMicroseconds, -2_000, 2_000);
        Assert.InRange(fromBob.Clock.OffsetMicroseconds, -2_000, 2_000);

        // A round trip on loopback is fast, but the figure only has to be plausible: it is a
        // quality signal for choosing between samples, not a measurement anything reports.
        Assert.InRange(fromAlice.Clock.RoundTripMicroseconds, 0, 100_000);
    }

    [Fact]
    public async Task DisconnectionIsReportedToBothSides()
    {
        await using var nodes = new TwoNodes();
        Assert.True(await nodes.ConnectAsync());
        await WaitForAsync(() => nodes.Bob.ConnectionCount > 0, TimeSpan.FromSeconds(10));

        var bobSawDisconnect = new TaskCompletionSource();
        nodes.Bob.PeerConnectionChanged += (_, connected) =>
        {
            if (!connected)
            {
                bobSawDisconnect.TrySetResult();
            }
        };

        await nodes.Alice.DisposeAsync();
        await bobSawDisconnect.Task.WaitAsync(TimeSpan.FromSeconds(10));

        Assert.Empty(nodes.Bob.ConnectedPeers);
    }
}
