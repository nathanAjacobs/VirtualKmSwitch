using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography.X509Certificates;
using VirtualKmSwitch.Core;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// Two peers over a real loopback socket, with real mutual TLS and real pinning. Only the network
/// is local; the handshake, the certificate validation and the framing are all genuine.
/// </summary>
public class PeerConnectionTests
{
    private sealed class Fixture : IAsyncDisposable
    {
        public X509Certificate2 AliceCertificate { get; } =
            NodeCertificate.Create("CN=Alice", StoreLocation.CurrentUser);

        public X509Certificate2 BobCertificate { get; } =
            NodeCertificate.Create("CN=Bob", StoreLocation.CurrentUser);

        public Guid AliceId { get; } = Guid.NewGuid();

        public Guid BobId { get; } = Guid.NewGuid();

        public PinStore AlicePins { get; } = new();

        public PinStore BobPins { get; } = new();

        /// <summary>Pins each side to the other, as a completed enrollment would.</summary>
        public void PinEachOther()
        {
            AlicePins.Pin(NodeCertificate.ThumbprintOf(BobCertificate), BobId);
            BobPins.Pin(NodeCertificate.ThumbprintOf(AliceCertificate), AliceId);
        }

        public ValueTask DisposeAsync()
        {
            AliceCertificate.Dispose();
            BobCertificate.Dispose();
            return ValueTask.CompletedTask;
        }
    }

    /// <summary>Establishes a connected, mutually authenticated pair on an ephemeral port.</summary>
    private static async Task<(PeerConnection Client, PeerConnection Server)> ConnectPairAsync(Fixture fixture)
    {
        var listener = new TcpListener(IPAddress.Loopback, 0);
        listener.Start();

        try
        {
            int port = ((IPEndPoint)listener.LocalEndpoint).Port;

            Task<TcpClient> accepting = listener.AcceptTcpClientAsync();

            Task<PeerConnection> clientTask = PeerConnection.ConnectAsync(
                IPAddress.Loopback.ToString(), port, fixture.AliceId, fixture.AliceCertificate, fixture.AlicePins);

            TcpClient accepted = await accepting;
            Task<PeerConnection> serverTask = PeerConnection.AcceptAsync(
                accepted, fixture.BobId, fixture.BobCertificate, fixture.BobPins);

            return (await clientTask, await serverTask);
        }
        finally
        {
            listener.Stop();
        }
    }

    [Fact]
    public async Task MutuallyPinnedPeersConnectAndIdentifyEachOther()
    {
        await using var fixture = new Fixture();
        fixture.PinEachOther();

        (PeerConnection client, PeerConnection server) = await ConnectPairAsync(fixture);

        await using (client)
        await using (server)
        {
            // Each side resolved the other's identity from its pinned certificate, not from
            // anything the peer claimed about itself.
            Assert.Equal(fixture.BobId, client.PeerId);
            Assert.Equal(fixture.AliceId, server.PeerId);
        }
    }

    [Fact]
    public async Task AnUnpinnedPeerIsRefused()
    {
        // The whole authorisation model: without a pin there is no CA to fall back on, so the
        // handshake must fail rather than accept a valid-looking self-signed certificate.
        await using var fixture = new Fixture();
        fixture.BobPins.Pin(NodeCertificate.ThumbprintOf(fixture.AliceCertificate), fixture.AliceId);

        // Alice does not pin Bob, so Alice must reject Bob's certificate.
        await Assert.ThrowsAnyAsync<Exception>(async () =>
        {
            (PeerConnection client, PeerConnection server) = await ConnectPairAsync(fixture);
            await client.DisposeAsync();
            await server.DisposeAsync();
        });
    }

    [Fact]
    public async Task APeerWithNoCertificatePinnedAtAllIsRefused()
    {
        await using var fixture = new Fixture();

        await Assert.ThrowsAnyAsync<Exception>(async () =>
        {
            (PeerConnection client, PeerConnection server) = await ConnectPairAsync(fixture);
            await client.DisposeAsync();
            await server.DisposeAsync();
        });
    }

    [Fact]
    public async Task InputEventsTravelAcrossTheEncryptedLink()
    {
        await using var fixture = new Fixture();
        fixture.PinEachOther();

        (PeerConnection client, PeerConnection server) = await ConnectPairAsync(fixture);

        await using (client)
        await using (server)
        {
            var received = new List<InputEvent>();
            var done = new TaskCompletionSource();
            using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(10));

            Task receiving = server.ReceiveLoopAsync(
                (_, events, _) =>
                {
                    foreach (ref readonly InputEvent inputEvent in events)
                    {
                        received.Add(inputEvent);
                    }

                    if (received.Count >= 3)
                    {
                        done.TrySetResult();
                    }
                },
                onControl: null,
                cancellation.Token);

            InputEvent[] batch =
            [
                InputEvent.MouseMove(100, 5, -5),
                InputEvent.Key(100, 0x1E, InputEventFlags.None),
                InputEvent.MouseButton(100, 0, up: true),
            ];

            // Measured as a delta: the connection has already exchanged a hello frame, which is
            // what proves the peer accepted it.
            long framesBefore = client.FramesSent;

            await client.SendInputAsync(100, batch);
            await done.Task.WaitAsync(TimeSpan.FromSeconds(10));

            Assert.Equal(3, received.Count);
            Assert.Equal(InputEventKind.MouseMove, received[0].Kind);
            Assert.Equal(5, received[0].X);
            Assert.Equal(0x1E, received[1].Code);
            Assert.True(received[2].IsUp);

            Assert.Equal(1, client.FramesSent - framesBefore);
            Assert.Equal(3, client.EventsSent);

            await cancellation.CancelAsync();
            await Task.WhenAny(receiving, Task.Delay(TimeSpan.FromSeconds(2)));
        }
    }

    [Fact]
    public async Task ManyBatchesArriveCompleteAndInOrder()
    {
        // Frames split and coalesce arbitrarily across TCP reads; ordering is what a key-up
        // depends on, so this is the property that matters most.
        await using var fixture = new Fixture();
        fixture.PinEachOther();

        (PeerConnection client, PeerConnection server) = await ConnectPairAsync(fixture);

        await using (client)
        await using (server)
        {
            const int batches = 500;
            const int perBatch = 8;

            var received = new List<int>(batches * perBatch);
            var done = new TaskCompletionSource();
            using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(20));

            Task receiving = server.ReceiveLoopAsync(
                (_, events, _) =>
                {
                    foreach (ref readonly InputEvent inputEvent in events)
                    {
                        received.Add(inputEvent.X);
                    }

                    if (received.Count >= batches * perBatch)
                    {
                        done.TrySetResult();
                    }
                },
                onControl: null,
                cancellation.Token);

            var buffer = new InputEvent[perBatch];
            int sequence = 0;

            for (int batch = 0; batch < batches; batch++)
            {
                for (int i = 0; i < perBatch; i++)
                {
                    buffer[i] = InputEvent.MouseMove(batch, sequence++, 0);
                }

                await client.SendInputAsync(batch, buffer);
            }

            await done.Task.WaitAsync(TimeSpan.FromSeconds(20));

            Assert.Equal(batches * perBatch, received.Count);
            for (int i = 0; i < received.Count; i++)
            {
                Assert.Equal(i, received[i]);
            }

            await cancellation.CancelAsync();
            await Task.WhenAny(receiving, Task.Delay(TimeSpan.FromSeconds(2)));
        }
    }

    [Fact]
    public async Task ControlPayloadsTravelAlongsideInput()
    {
        await using var fixture = new Fixture();
        fixture.PinEachOther();

        (PeerConnection client, PeerConnection server) = await ConnectPairAsync(fixture);

        await using (client)
        await using (server)
        {
            byte[]? seen = null;
            var done = new TaskCompletionSource();
            using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(10));

            Task receiving = server.ReceiveLoopAsync(
                onInput: null,
                onControl: (_, payload) =>
                {
                    seen = payload.ToArray();
                    done.TrySetResult();
                },
                cancellation.Token);

            byte[] message = "hello"u8.ToArray();
            await client.SendControlAsync(message);
            await done.Task.WaitAsync(TimeSpan.FromSeconds(10));

            Assert.Equal(message, seen);

            await cancellation.CancelAsync();
            await Task.WhenAny(receiving, Task.Delay(TimeSpan.FromSeconds(2)));
        }
    }

    [Fact]
    public async Task LatencyIsMeasuredOnReceive()
    {
        await using var fixture = new Fixture();
        fixture.PinEachOther();

        (PeerConnection client, PeerConnection server) = await ConnectPairAsync(fixture);

        await using (client)
        await using (server)
        {
            var done = new TaskCompletionSource();
            using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(10));

            Task receiving = server.ReceiveLoopAsync(
                (_, _, _) => done.TrySetResult(), onControl: null, cancellation.Token);

            // Microseconds, which is what the wire carries. Handing it Stopwatch ticks is exactly
            // the mistake the shared clock exists to prevent, and it reads as a plausible number.
            await client.SendInputAsync(
                MonotonicClock.Microseconds, new[] { InputEvent.MouseMove(0, 1, 1) });

            await done.Task.WaitAsync(TimeSpan.FromSeconds(10));

            Assert.Equal(1, server.Latency.Count);
            Assert.True(server.Latency.MaxMicroseconds >= 0);

            await cancellation.CancelAsync();
            await Task.WhenAny(receiving, Task.Delay(TimeSpan.FromSeconds(2)));
        }
    }
}

public class LatencyTrackerTests
{
    [Fact]
    public void PercentilesReflectTheDistribution()
    {
        var tracker = new LatencyTracker();

        // 99 fast samples and one very slow one: the mean would hide the outlier entirely, which
        // is exactly why the budget is stated as a p99.
        for (int i = 0; i < 99; i++)
        {
            tracker.Record(100);
        }

        tracker.Record(50_000);

        Assert.Equal(100, tracker.Count);
        Assert.True(tracker.Percentile(0.50) <= 200);
        Assert.True(tracker.Percentile(0.99) <= 200);
        Assert.True(tracker.Percentile(1.0) >= 50_000);
        Assert.Equal(50_000, tracker.MaxMicroseconds);
    }

    [Fact]
    public void NegativeSamplesAreDiscarded()
    {
        // A negative measurement means the clock offset estimate is stale; counting it would drag
        // the percentiles below anything achievable.
        var tracker = new LatencyTracker();
        tracker.Record(-500);

        Assert.Equal(0, tracker.Count);
    }

    [Fact]
    public void ResolutionIsFinestWhereTheBudgetLives()
    {
        var tracker = new LatencyTracker();
        tracker.Record(45);

        // Sub-millisecond samples are bucketed to 10 microseconds.
        Assert.InRange(tracker.Percentile(0.5), 45, 55);
    }

    [Fact]
    public void RecordingAllocatesNothing()
    {
        var tracker = new LatencyTracker();
        tracker.Record(100);

        long before = GC.GetAllocatedBytesForCurrentThread();
        for (int i = 0; i < 10_000; i++)
        {
            tracker.Record(i % 5_000);
        }

        Assert.Equal(0, GC.GetAllocatedBytesForCurrentThread() - before);
    }

    [Fact]
    public void EmptyTrackerReportsZeroRatherThanFailing()
    {
        var tracker = new LatencyTracker();

        Assert.Equal(0, tracker.Count);
        Assert.Equal(0, tracker.Percentile(0.99));
        Assert.Equal(0, tracker.MeanMicroseconds);
        Assert.Equal("no samples", tracker.Summary());
    }
}
