using System.Diagnostics;
using VirtualKmSwitch.Core;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

/// <summary>
/// The outbound queue, its batching, and the one policy question that matters: what is allowed to
/// be lost when the link cannot keep up.
/// </summary>
public class PeerSendPumpTests
{
    /// <summary>A sink that records what it was handed, and can be made slow on demand.</summary>
    private sealed class RecordingSink : IInputSink
    {
        private readonly Lock _gate = new();
        private readonly List<InputEvent[]> _batches = [];

        /// <summary>Held closed to let the queue build up behind a send in progress.</summary>
        public ManualResetEventSlim Release { get; } = new(initialState: true);

        public bool Fail { get; set; }

        public int BatchCount
        {
            get
            {
                lock (_gate)
                {
                    return _batches.Count;
                }
            }
        }

        public IReadOnlyList<InputEvent[]> Batches
        {
            get
            {
                lock (_gate)
                {
                    return _batches.ToArray();
                }
            }
        }

        public InputEvent[] AllEvents
        {
            get
            {
                lock (_gate)
                {
                    return _batches.SelectMany(batch => batch).ToArray();
                }
            }
        }

        public bool TrySendInput(long captureMicroseconds, ReadOnlySpan<InputEvent> events)
        {
            Release.Wait();

            if (Fail)
            {
                return false;
            }

            InputEvent[] copy = events.ToArray();
            lock (_gate)
            {
                _batches.Add(copy);
            }

            return true;
        }
    }

    private static async Task WaitForAsync(Func<bool> condition, string what)
    {
        var deadline = Stopwatch.StartNew();
        while (deadline.Elapsed < TimeSpan.FromSeconds(10))
        {
            if (condition())
            {
                return;
            }

            await Task.Delay(5);
        }

        Assert.Fail("timed out waiting for " + what);
    }

    [Fact]
    public async Task AtIdleAnEventGoesOutOnItsOwnRatherThanWaitingForCompany()
    {
        // The property that separates coalescing from buffering. A pump that waited for a second
        // event before sending would post better throughput numbers and feel worse to use.
        var sink = new RecordingSink();
        using var pump = new PeerSendPump(sink, "idle");
        pump.Start();

        Assert.True(pump.TryEnqueue(InputEvent.Key(1, 30, InputEventFlags.None)));

        await WaitForAsync(() => sink.BatchCount == 1, "the lone event to be sent");
        Assert.Single(sink.Batches[0]);
    }

    [Fact]
    public async Task BatchesEmergeFromTheDrainWhenTheLinkIsBusy()
    {
        var sink = new RecordingSink();
        using var pump = new PeerSendPump(sink, "busy");

        // Hold the sink shut so the pump blocks inside its first send while events pile up. This
        // is what a slow write looks like from the queue's side.
        sink.Release.Reset();
        pump.Start();

        pump.TryEnqueue(InputEvent.Key(1, 1, InputEventFlags.None));
        await Task.Delay(30);

        for (int i = 0; i < 200; i++)
        {
            Assert.True(pump.TryEnqueue(InputEvent.Key(i + 2, (ushort)(i % 60), InputEventFlags.None)));
        }

        sink.Release.Set();

        await WaitForAsync(() => sink.AllEvents.Length == 201, "every event to arrive");

        // Nothing was dropped, and the events that queued during the stall left in far fewer
        // frames than they arrived in -- one syscall and one TLS record instead of 200.
        Assert.Equal(201, sink.AllEvents.Length);
        Assert.True(pump.LargestBatch > 1, $"largest batch was {pump.LargestBatch}; no batching happened");
        Assert.True(
            sink.BatchCount < 201,
            $"{sink.BatchCount} frames for 201 events means each one went out alone");
    }

    [Fact]
    public async Task MotionMergesButAClickNeverCrossesIt()
    {
        var sink = new RecordingSink();
        using var pump = new PeerSendPump(sink, "merge");

        sink.Release.Reset();
        pump.Start();

        // Prime the pump so it is blocked in a send, then queue a gesture behind it.
        pump.TryEnqueue(InputEvent.Key(1, 1, InputEventFlags.None));
        await Task.Delay(30);

        pump.TryEnqueue(InputEvent.MouseMove(10, 3, 0));
        pump.TryEnqueue(InputEvent.MouseMove(11, 4, 0));
        pump.TryEnqueue(InputEvent.MouseButton(12, 0, up: false));
        pump.TryEnqueue(InputEvent.MouseMove(13, 5, 0));
        pump.TryEnqueue(InputEvent.MouseButton(14, 0, up: true));

        sink.Release.Set();

        await WaitForAsync(
            () => sink.AllEvents.Count(e => e.Kind == InputEventKind.MouseButton) == 2,
            "both button transitions");

        InputEvent[] gesture = sink.AllEvents.Where(e => e.Kind != InputEventKind.Key).ToArray();

        // Motion before the press is one event carrying the sum, and the press still lands after
        // it. A drag that reordered here would click somewhere the user never pointed.
        Assert.Equal(InputEventKind.MouseMove, gesture[0].Kind);
        Assert.Equal(7, gesture[0].X);
        Assert.Equal(InputEventKind.MouseButton, gesture[1].Kind);
        Assert.False(gesture[1].IsUp);
        Assert.Equal(InputEventKind.MouseMove, gesture[2].Kind);
        Assert.Equal(5, gesture[2].X);
        Assert.Equal(InputEventKind.MouseButton, gesture[3].Kind);
        Assert.True(gesture[3].IsUp);
    }

    [Fact]
    public void AFullQueueDropsMotionAndSaysItSucceeded()
    {
        // The pump is never started, so nothing drains and the queue fills.
        var sink = new RecordingSink();
        using var pump = new PeerSendPump(sink, "full-motion", capacity: 8);

        for (int i = 0; i < 8; i++)
        {
            Assert.True(pump.TryEnqueue(InputEvent.MouseMove(i, 1, 0)));
        }

        // Accepted, because the motion after it supersedes it: the cursor still ends up in the
        // right place, and a hitch in movement is a far cheaper failure than a stuck modifier.
        Assert.True(pump.TryEnqueue(InputEvent.MouseMove(9, 1, 0)));
        Assert.Equal(1, pump.MovesDropped);
    }

    [Fact]
    public void AFullQueueRefusesAKeyEventAndReportsTheStall()
    {
        var sink = new RecordingSink();
        using var pump = new PeerSendPump(sink, "full-keys", capacity: 8);

        SendPumpStall? stall = null;
        pump.Stalled += reason => stall = reason;

        for (int i = 0; i < 8; i++)
        {
            Assert.True(pump.TryEnqueue(InputEvent.Key(i, (ushort)i, InputEventFlags.None)));
        }

        // Refused rather than dropped. Losing one of these is a coin flip between a key that never
        // arrives and a key-up that never arrives, and the second leaves a modifier held down on a
        // machine the user may already have stopped driving.
        Assert.False(pump.TryEnqueue(InputEvent.Key(9, 9, InputEventFlags.None)));
        Assert.Equal(0, pump.MovesDropped);
        Assert.Equal(SendPumpStall.QueueFull, stall);
    }

    [Fact]
    public void AReleaseAllIsNeverTreatedAsDroppable()
    {
        var sink = new RecordingSink();
        using var pump = new PeerSendPump(sink, "release-all", capacity: 8);

        for (int i = 0; i < 8; i++)
        {
            pump.TryEnqueue(InputEvent.MouseMove(i, 1, 0));
        }

        // The event whose entire purpose is to clean up after the others.
        Assert.False(pump.TryEnqueue(InputEvent.ReleaseAll(9)));
        Assert.Equal(0, pump.MovesDropped);
    }

    [Fact]
    public async Task ALinkThatDiesIsReportedRatherThanRetriedForever()
    {
        var sink = new RecordingSink { Fail = true };
        using var pump = new PeerSendPump(sink, "dead");

        SendPumpStall? stall = null;
        pump.Stalled += reason => stall = reason;
        pump.Start();

        pump.TryEnqueue(InputEvent.Key(1, 1, InputEventFlags.None));

        await WaitForAsync(() => stall is not null, "the link loss to be reported");
        Assert.Equal(SendPumpStall.LinkLost, stall);
    }

    [Fact]
    public void QueueingAnEventAllocatesNothing()
    {
        // The assertion that keeps the hot path honest as the code grows. A single allocation here
        // is a gen0 collection every few seconds of typing, and a gen0 collection near the hook
        // thread is how Windows silently unhooks us.
        var sink = new RecordingSink();
        using var pump = new PeerSendPump(sink, "alloc", capacity: 1024);

        var warmup = InputEvent.MouseMove(1, 1, 1);
        pump.TryEnqueue(in warmup);

        long before = GC.GetAllocatedBytesForCurrentThread();

        for (int i = 0; i < 1000; i++)
        {
            var moved = InputEvent.MouseMove(i, i, i);
            pump.TryEnqueue(in moved);
        }

        long allocated = GC.GetAllocatedBytesForCurrentThread() - before;
        Assert.Equal(0, allocated);
    }
}
