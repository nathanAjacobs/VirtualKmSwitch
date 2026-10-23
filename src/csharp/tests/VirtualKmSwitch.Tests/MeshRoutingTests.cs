using System.Diagnostics;
using System.Security.Cryptography.X509Certificates;
using Microsoft.Extensions.Logging.Abstractions;
using VirtualKmSwitch.Core;
using VirtualKmSwitch.Service;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// A keystroke captured on one machine arriving on another, over a real encrypted link.
///
/// This composes the same pieces the service composes -- a router, a mesh, and the adapter between
/// them -- rather than mocking the join between them, because that join is the part that has never
/// run before and is where the wiring can be wrong while every component test still passes.
/// </summary>
public class MeshRoutingTests
{
    /// <summary>The adapter <see cref="MeshHost"/> is, reduced to the part under test.</summary>
    private sealed class MeshSender(PeerMesh mesh) : IPeerSender
    {
        public bool TrySend(Guid peerId, in InputEvent inputEvent) => mesh.TrySend(peerId, in inputEvent);

        public bool IsConnected(Guid peerId) => mesh.Connection(peerId) is not null;
    }

    private sealed class TwoMachines : IAsyncDisposable
    {
        public TwoMachines()
        {
            AliceCertificate = NodeCertificate.Create("CN=RouteAlice", StoreLocation.CurrentUser);
            BobCertificate = NodeCertificate.Create("CN=RouteBob", StoreLocation.CurrentUser);

            // Ordered so Alice is the dialler, matching the lower-id-dials rule.
            Guid first = Guid.NewGuid();
            Guid second = Guid.NewGuid();
            (AliceId, BobId) = first.CompareTo(second) < 0 ? (first, second) : (second, first);

            var alicePins = new PinStore();
            var bobPins = new PinStore();
            alicePins.Pin(NodeCertificate.ThumbprintOf(BobCertificate), BobId);
            bobPins.Pin(NodeCertificate.ThumbprintOf(AliceCertificate), AliceId);

            int port = Random.Shared.Next(45_000, 60_000);
            AlicePort = port;
            BobPort = port + 1;

            AliceMesh = new PeerMesh(AliceId, AliceCertificate, alicePins, AlicePort);
            BobMesh = new PeerMesh(BobId, BobCertificate, bobPins, BobPort);

            AliceRouter = new InputRouter(
                new AgentSupervisor(NullLogger<AgentSupervisor>.Instance),
                NullLogger<InputRouter>.Instance)
            {
                Sender = new MeshSender(AliceMesh),
            };

            BobMesh.InputReceived += (_, batch, _) =>
            {
                lock (Received)
                {
                    foreach (InputEvent inputEvent in batch)
                    {
                        Received.Add(inputEvent);
                    }
                }
            };
        }

        public X509Certificate2 AliceCertificate { get; }

        public X509Certificate2 BobCertificate { get; }

        public Guid AliceId { get; }

        public Guid BobId { get; }

        public int AlicePort { get; }

        public int BobPort { get; }

        public PeerMesh AliceMesh { get; }

        public PeerMesh BobMesh { get; }

        public InputRouter AliceRouter { get; }

        /// <summary>What actually landed on Bob.</summary>
        public List<InputEvent> Received { get; } = [];

        public InputEvent[] Snapshot()
        {
            lock (Received)
            {
                return [.. Received];
            }
        }

        public async Task ConnectAsync()
        {
            AliceMesh.Start();
            BobMesh.Start();

            Assert.True(await AliceMesh.EnsureConnectedAsync(BobId, "127.0.0.1", BobPort));
            await WaitForAsync(() => BobMesh.ConnectionCount > 0, "Bob to see the link");
        }

        public async ValueTask DisposeAsync()
        {
            await AliceMesh.DisposeAsync();
            await BobMesh.DisposeAsync();
            AliceCertificate.Dispose();
            BobCertificate.Dispose();
        }
    }

    private static async Task WaitForAsync(Func<bool> condition, string what)
    {
        var deadline = Stopwatch.StartNew();
        while (deadline.Elapsed < TimeSpan.FromSeconds(15))
        {
            if (condition())
            {
                return;
            }

            await Task.Delay(10);
        }

        Assert.Fail("timed out waiting for " + what);
    }

    [Fact]
    public async Task AKeystrokeCapturedWhileEngagedArrivesOnTheMachineBeingDriven()
    {
        await using var machines = new TwoMachines();
        await machines.ConnectAsync();

        machines.AliceRouter.Engage(machines.BobId);

        machines.AliceRouter.Capture(InputEvent.Key(Stopwatch.GetTimestamp(), 30, InputEventFlags.None));
        machines.AliceRouter.Capture(InputEvent.Key(Stopwatch.GetTimestamp(), 30, InputEventFlags.Up));

        await WaitForAsync(() => machines.Snapshot().Length >= 2, "both key transitions to arrive");

        InputEvent[] received = machines.Snapshot();

        Assert.Equal(InputEventKind.Key, received[0].Kind);
        Assert.Equal(30, received[0].Code);
        Assert.False(received[0].IsUp);

        // The release matters as much as the press. It is the half that, when lost, leaves a key
        // held down on a machine the user cannot reach from where they are sitting.
        Assert.True(received[1].IsUp);
    }

    [Fact]
    public async Task NothingCrossesTheLinkUntilTheMachineIsEngaged()
    {
        // The link exists from the moment both machines are members. Being connected must not be
        // the same as being driven, or every peer would type into every other one.
        await using var machines = new TwoMachines();
        await machines.ConnectAsync();

        machines.AliceRouter.Capture(InputEvent.Key(Stopwatch.GetTimestamp(), 30, InputEventFlags.None));
        await Task.Delay(250);

        Assert.Empty(machines.Snapshot());
    }

    [Fact]
    public async Task DisengagingSendsTheReleaseBeforeControlComesBack()
    {
        await using var machines = new TwoMachines();
        await machines.ConnectAsync();

        machines.AliceRouter.Engage(machines.BobId);
        machines.AliceRouter.Capture(InputEvent.Key(Stopwatch.GetTimestamp(), 42, InputEventFlags.None));
        await WaitForAsync(() => machines.Snapshot().Length >= 1, "the keypress to arrive");

        machines.AliceRouter.Disengage();

        await WaitForAsync(
            () => machines.Snapshot().Any(e => e.Kind == InputEventKind.ReleaseAll),
            "the release to reach the machine being left");

        Assert.False(machines.AliceRouter.IsEngaged);
    }

    [Fact]
    public async Task MouseMotionMergesOnTheWayOutButTheClickStaysBehindIt()
    {
        // Proves the coalescer is genuinely in this path rather than merely compiled into it, and
        // that the ordering rule survives the whole journey: a drag must land where it was aimed.
        await using var machines = new TwoMachines();
        await machines.ConnectAsync();

        machines.AliceRouter.Engage(machines.BobId);

        for (int i = 0; i < 50; i++)
        {
            machines.AliceRouter.Capture(InputEvent.MouseMove(Stopwatch.GetTimestamp(), 2, 1));
        }

        machines.AliceRouter.Capture(InputEvent.MouseButton(Stopwatch.GetTimestamp(), 0, up: false));

        await WaitForAsync(
            () => machines.Snapshot().Any(e => e.Kind == InputEventKind.MouseButton),
            "the click to arrive");

        InputEvent[] received = machines.Snapshot();
        InputEvent[] motion = [.. received.Where(e => e.Kind == InputEventKind.MouseMove)];

        // Every pixel of movement is still there; it simply travelled in fewer events.
        Assert.Equal(100, motion.Sum(e => e.X));
        Assert.Equal(50, motion.Sum(e => e.Y));

        int click = Array.FindIndex(received, e => e.Kind == InputEventKind.MouseButton);
        Assert.Equal(received.Length - 1, click);
    }

    [Fact]
    public async Task InputFlowsBothWaysAtOnceBecauseEitherPeerMayBeDriving()
    {
        // A can drive B while C drives A, so a link is never one-directional. Only loopback
        // suppression makes that safe, and it only matters if both directions really work.
        await using var machines = new TwoMachines();
        await machines.ConnectAsync();

        var backToAlice = new List<InputEvent>();
        machines.AliceMesh.InputReceived += (_, batch, _) =>
        {
            lock (backToAlice)
            {
                foreach (InputEvent inputEvent in batch)
                {
                    backToAlice.Add(inputEvent);
                }
            }
        };

        machines.AliceRouter.Engage(machines.BobId);
        machines.AliceRouter.Capture(InputEvent.Key(Stopwatch.GetTimestamp(), 30, InputEventFlags.None));

        Assert.True(machines.BobMesh.TrySend(
            machines.AliceId, InputEvent.Key(Stopwatch.GetTimestamp(), 31, InputEventFlags.None)));

        await WaitForAsync(() => machines.Snapshot().Length >= 1, "Alice's keystroke to reach Bob");

        await WaitForAsync(
            () =>
            {
                lock (backToAlice)
                {
                    return backToAlice.Count >= 1;
                }
            },
            "Bob's keystroke to reach Alice");
    }
}
