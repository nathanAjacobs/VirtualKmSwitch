using Microsoft.Extensions.Logging.Abstractions;
using VirtualKmSwitch.Core;
using VirtualKmSwitch.Service;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// Which machine a captured keystroke goes to.
///
/// The whole product is this decision made correctly a few thousand times a second, and both ways
/// of getting it wrong are bad in ways a user cannot recover from: input that leaks to the local
/// machine while they believe they are driving a remote one, or a modifier left held down on a
/// machine they have stopped driving.
/// </summary>
public class InputRouterTests
{
    private sealed class FakeSender : IPeerSender
    {
        private readonly List<(Guid Peer, InputEvent Event)> _sent = [];

        /// <summary>Peers this sender will refuse, standing in for a queue that has backed up.</summary>
        public HashSet<Guid> Refusing { get; } = [];

        public HashSet<Guid> Connected { get; } = [];

        public IReadOnlyList<(Guid Peer, InputEvent Event)> Sent => _sent;

        public bool TrySend(Guid peerId, in InputEvent inputEvent)
        {
            if (Refusing.Contains(peerId))
            {
                return false;
            }

            _sent.Add((peerId, inputEvent));
            return true;
        }

        public bool IsConnected(Guid peerId) => Connected.Contains(peerId);
    }

    private static InputRouter Create(FakeSender sender)
    {
        var router = new InputRouter(
            new AgentSupervisor(NullLogger<AgentSupervisor>.Instance),
            NullLogger<InputRouter>.Instance);

        router.Sender = sender;
        return router;
    }

    [Fact]
    public void WithNothingEngagedNothingLeavesTheMachine()
    {
        // The default state, and the one a bug here would be most damaging in: an idle machine
        // that quietly forwards its owner's typing somewhere.
        var sender = new FakeSender();
        InputRouter router = Create(sender);

        router.Capture(InputEvent.Key(1, 30, InputEventFlags.None));
        router.Capture(InputEvent.MouseMove(2, 5, 5));

        Assert.Empty(sender.Sent);
        Assert.False(router.IsEngaged);
        Assert.Equal(Guid.Empty, router.Target);
    }

    [Fact]
    public void WhileEngagedEveryCapturedEventGoesToThatMachine()
    {
        var sender = new FakeSender();
        InputRouter router = Create(sender);
        var peer = Guid.NewGuid();

        router.Engage(peer);
        router.Capture(InputEvent.Key(1, 30, InputEventFlags.None));
        router.Capture(InputEvent.MouseMove(2, 5, 5));
        router.Capture(InputEvent.Key(3, 30, InputEventFlags.Up));

        Assert.Equal(3, sender.Sent.Count);
        Assert.All(sender.Sent, entry => Assert.Equal(peer, entry.Peer));
        Assert.Equal(3, router.SentToPeer);
    }

    [Fact]
    public void EngagingASecondMachineRedirectsRatherThanDuplicating()
    {
        var sender = new FakeSender();
        InputRouter router = Create(sender);
        var first = Guid.NewGuid();
        var second = Guid.NewGuid();

        router.Engage(first);
        router.Capture(InputEvent.Key(1, 30, InputEventFlags.None));

        router.Engage(second);
        router.Capture(InputEvent.Key(2, 31, InputEventFlags.None));

        Assert.Equal(first, sender.Sent[0].Peer);
        Assert.Equal(second, sender.Sent[1].Peer);
        Assert.Equal(second, router.Target);
    }

    [Fact]
    public void DisengagingReleasesEverythingHeldOnTheMachineBeingLeft()
    {
        // The single most common way a KVM breaks: a modifier is down when control moves away, and
        // the machine left behind has no way to work out that it should let go.
        var sender = new FakeSender();
        InputRouter router = Create(sender);
        var peer = Guid.NewGuid();

        router.Engage(peer);
        router.Capture(InputEvent.Key(1, 42, InputEventFlags.None));
        router.Disengage();

        (Guid Peer, InputEvent Event) last = sender.Sent[^1];

        Assert.Equal(peer, last.Peer);
        Assert.Equal(InputEventKind.ReleaseAll, last.Event.Kind);
        Assert.False(router.IsEngaged);
    }

    [Fact]
    public void DisengagingWhenNothingIsEngagedIsHarmless()
    {
        // Reached by a person who is not sure what state they are in, which is exactly when it
        // must not throw or send anything.
        var sender = new FakeSender();
        InputRouter router = Create(sender);

        router.Disengage();
        router.Disengage();

        Assert.Empty(sender.Sent);
    }

    [Fact]
    public void OnceDisengagedInputStaysOnThisMachine()
    {
        var sender = new FakeSender();
        InputRouter router = Create(sender);
        var peer = Guid.NewGuid();

        router.Engage(peer);
        router.Disengage();

        int afterRelease = sender.Sent.Count;
        router.Capture(InputEvent.Key(9, 30, InputEventFlags.None));

        Assert.Equal(afterRelease, sender.Sent.Count);
    }

    [Fact]
    public void ATargetThatCannotTakeAKeyEventIsReported()
    {
        var sender = new FakeSender();
        InputRouter router = Create(sender);
        var peer = Guid.NewGuid();
        sender.Refusing.Add(peer);

        Guid? stalled = null;
        router.TargetStalled += id => stalled = id;

        router.Engage(peer);
        router.Capture(InputEvent.Key(1, 30, InputEventFlags.None));

        // Surfaced rather than counted. The listener disengages, which is the only response that
        // does not leave the remote machine in a state nothing can clear.
        Assert.Equal(peer, stalled);
        Assert.Equal(1, router.RefusedByPeer);
    }

    [Fact]
    public void WithNoMeshAttachedEngagingIsInertRatherThanFatal()
    {
        // The service starts before the mesh does, and a hotkey pressed in that window must not
        // take the router down.
        var router = new InputRouter(
            new AgentSupervisor(NullLogger<AgentSupervisor>.Instance),
            NullLogger<InputRouter>.Instance);

        router.Engage(Guid.NewGuid());
        router.Capture(InputEvent.Key(1, 30, InputEventFlags.None));
        router.Disengage();

        Assert.False(router.IsEngaged);
    }

    [Fact]
    public void CapturedEventsAreStillReportedLocallyWhileEngaged()
    {
        // Diagnostics must not go dark the moment the machine starts driving another one.
        var sender = new FakeSender();
        InputRouter router = Create(sender);

        int seen = 0;
        router.CapturedLocally += _ => seen++;

        router.Engage(Guid.NewGuid());
        router.Capture(InputEvent.Key(1, 30, InputEventFlags.None));
        router.Capture(InputEvent.Key(2, 30, InputEventFlags.Up));

        Assert.Equal(2, seen);
    }
}
