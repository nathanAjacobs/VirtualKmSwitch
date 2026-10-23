using VirtualKmSwitch.Core;
using VirtualKmSwitch.Transport;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class InputCoalescerTests
{
    private static int Run(InputEvent[] source, out InputEvent[] result)
    {
        var destination = new InputEvent[source.Length];
        int count = InputCoalescer.Coalesce(source, destination);
        result = destination[..count];
        return count;
    }

    [Fact]
    public void ConsecutiveMotionMergesIntoOneEvent()
    {
        InputEvent[] source =
        [
            InputEvent.MouseMove(10, 3, 1),
            InputEvent.MouseMove(20, 4, 2),
            InputEvent.MouseMove(30, -1, 3),
        ];

        Assert.Equal(1, Run(source, out InputEvent[] result));
        Assert.Equal(6, result[0].X);
        Assert.Equal(6, result[0].Y);

        // The earliest timestamp survives, so latency is measured from when the movement began
        // rather than when the batch happened to be flushed.
        Assert.Equal(10, result[0].Timestamp);
    }

    [Fact]
    public void ButtonsAndKeysEndTheRun()
    {
        // A click must stay after the motion that preceded it, or a drag lands somewhere else.
        InputEvent[] source =
        [
            InputEvent.MouseMove(1, 5, 0),
            InputEvent.MouseMove(2, 5, 0),
            InputEvent.MouseButton(3, 0, up: false),
            InputEvent.MouseMove(4, 7, 0),
            InputEvent.MouseButton(5, 0, up: true),
        ];

        Assert.Equal(4, Run(source, out InputEvent[] result));
        Assert.Equal(InputEventKind.MouseMove, result[0].Kind);
        Assert.Equal(10, result[0].X);
        Assert.Equal(InputEventKind.MouseButton, result[1].Kind);
        Assert.False(result[1].IsUp);
        Assert.Equal(7, result[2].X);
        Assert.True(result[3].IsUp);
    }

    [Fact]
    public void KeyEventsAreNeverMergedOrDropped()
    {
        // Losing a key-up strands a modifier held down on a remote machine, which is the one
        // failure that cannot be recovered from the far end.
        InputEvent[] source =
        [
            InputEvent.Key(1, 0x2A, InputEventFlags.None),
            InputEvent.Key(2, 0x1E, InputEventFlags.None),
            InputEvent.Key(3, 0x1E, InputEventFlags.Up),
            InputEvent.Key(4, 0x2A, InputEventFlags.Up),
        ];

        Assert.Equal(4, Run(source, out InputEvent[] result));
        Assert.Equal(source, result);
    }

    [Fact]
    public void MotionThatCancelsOutIsDropped()
    {
        // A net-zero run is not movement; sending it would make the far side inject nothing at
        // some cost.
        InputEvent[] source =
        [
            InputEvent.MouseMove(1, 5, 5),
            InputEvent.MouseMove(2, -5, -5),
        ];

        Assert.Equal(0, Run(source, out _));
    }

    [Fact]
    public void ACancellingRunBetweenClicksStillPreservesTheClicks()
    {
        InputEvent[] source =
        [
            InputEvent.MouseButton(1, 0, up: false),
            InputEvent.MouseMove(2, 3, 0),
            InputEvent.MouseMove(3, -3, 0),
            InputEvent.MouseButton(4, 0, up: true),
        ];

        Assert.Equal(2, Run(source, out InputEvent[] result));
        Assert.All(result, e => Assert.Equal(InputEventKind.MouseButton, e.Kind));
    }

    [Fact]
    public void AccumulatedMotionSaturatesRatherThanWrapping()
    {
        // Wrapping would send the pointer hard in the opposite direction, which is far worse than
        // clamping at a distance no screen is that wide.
        InputEvent[] source =
        [
            InputEvent.MouseMove(1, int.MaxValue, 0),
            InputEvent.MouseMove(2, int.MaxValue, 0),
        ];

        Assert.Equal(1, Run(source, out InputEvent[] result));
        Assert.Equal(int.MaxValue, result[0].X);
    }

    [Fact]
    public void OtherEventKindsPassThroughUntouched()
    {
        InputEvent[] source =
        [
            InputEvent.MouseWheel(1, 120, horizontal: false),
            InputEvent.ReleaseAll(2),
        ];

        Assert.Equal(2, Run(source, out InputEvent[] result));
        Assert.Equal(InputEventKind.MouseWheel, result[0].Kind);
        Assert.Equal(InputEventKind.ReleaseAll, result[1].Kind);
    }

    [Fact]
    public void EmptyInputProducesNothing() =>
        Assert.Equal(0, InputCoalescer.Coalesce([], new InputEvent[4]));

    [Fact]
    public void CoalescingAllocatesNothing()
    {
        var source = new InputEvent[64];
        for (int i = 0; i < source.Length; i++)
        {
            source[i] = i % 8 == 7
                ? InputEvent.Key(i, 0x1E, InputEventFlags.None)
                : InputEvent.MouseMove(i, 1, 1);
        }

        var destination = new InputEvent[64];
        InputCoalescer.Coalesce(source, destination);

        long before = GC.GetAllocatedBytesForCurrentThread();
        for (int i = 0; i < 10_000; i++)
        {
            InputCoalescer.Coalesce(source, destination);
        }

        Assert.Equal(0, GC.GetAllocatedBytesForCurrentThread() - before);
    }

    [Fact]
    public void MotionHeavyTrafficCompressesSharply()
    {
        // The realistic case: a high-polling-rate mouse produces long runs of tiny deltas, and
        // this is where batching earns its keep on the wire.
        var source = new InputEvent[1000];
        for (int i = 0; i < source.Length; i++)
        {
            source[i] = InputEvent.MouseMove(i, 1, 0);
        }

        Assert.Equal(1, Run(source, out InputEvent[] result));
        Assert.Equal(1000, result[0].X);
    }
}
