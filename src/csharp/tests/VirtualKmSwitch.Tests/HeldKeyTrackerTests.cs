using VirtualKmSwitch.Core;
using VirtualKmSwitch.InputEngine;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class HeldKeyTrackerTests
{
    private const ushort ScanLeftShift = 0x2A;
    private const ushort ScanA = 0x1E;
    private const ushort ScanRightCtrl = 0x1D;

    private static InputEvent KeyDown(ushort scanCode, InputEventFlags flags = InputEventFlags.None) =>
        InputEvent.Key(1, scanCode, flags);

    private static InputEvent KeyUp(ushort scanCode, InputEventFlags flags = InputEventFlags.None) =>
        InputEvent.Key(1, scanCode, flags | InputEventFlags.Up);

    [Fact]
    public void HeldKeysAreReleasedAndTheSetIsCleared()
    {
        var tracker = new HeldKeyTracker();
        tracker.Record(KeyDown(ScanLeftShift));
        tracker.Record(KeyDown(ScanA));

        Assert.Equal(2, tracker.HeldCount);

        Span<InputEvent> releases = new InputEvent[tracker.MaxReleaseEvents];
        int count = tracker.BuildReleaseAll(99, releases);

        Assert.Equal(2, count);
        Assert.All(releases[..count].ToArray(), e => Assert.True(e.IsUp));
        Assert.All(releases[..count].ToArray(), e => Assert.Equal(99, e.Timestamp));
        Assert.Equal(0, tracker.HeldCount);

        // A second call has nothing left to release.
        Assert.Equal(0, tracker.BuildReleaseAll(100, releases));
    }

    [Fact]
    public void ReleasedKeysAreNoLongerHeld()
    {
        var tracker = new HeldKeyTracker();
        tracker.Record(KeyDown(ScanA));
        tracker.Record(KeyUp(ScanA));

        Assert.Equal(0, tracker.HeldCount);

        Span<InputEvent> releases = new InputEvent[tracker.MaxReleaseEvents];
        Assert.Equal(0, tracker.BuildReleaseAll(1, releases));
    }

    [Fact]
    public void ExtendedKeysAreTrackedSeparatelyFromTheirBaseScancode()
    {
        // Right Ctrl and left Ctrl share scancode 0x1D and differ only by the extended flag.
        // Collapsing them would release the wrong key, or miss one entirely.
        var tracker = new HeldKeyTracker();
        tracker.Record(KeyDown(ScanRightCtrl, InputEventFlags.Extended));
        tracker.Record(KeyDown(ScanRightCtrl));

        Assert.Equal(2, tracker.HeldCount);

        Span<InputEvent> releases = new InputEvent[tracker.MaxReleaseEvents];
        int count = tracker.BuildReleaseAll(1, releases);

        Assert.Equal(2, count);
        Assert.Contains(releases[..count].ToArray(), e => (e.Flags & InputEventFlags.Extended) != 0);
        Assert.Contains(releases[..count].ToArray(), e => (e.Flags & InputEventFlags.Extended) == 0);
    }

    [Fact]
    public void VirtualKeysAreReleasedAsVirtualKeys()
    {
        // Pause travels as a virtual key because its scancode sequence cannot be replayed, so its
        // release has to go back the same way.
        var tracker = new HeldKeyTracker();
        tracker.Record(new InputEvent(1, InputEventKind.Key, InputEventFlags.VirtualKey, 0x13, 0, 0));

        Span<InputEvent> releases = new InputEvent[tracker.MaxReleaseEvents];
        int count = tracker.BuildReleaseAll(1, releases);

        Assert.Equal(1, count);
        Assert.True((releases[0].Flags & InputEventFlags.VirtualKey) != 0);
        Assert.True(releases[0].IsUp);
        Assert.Equal(0x13, releases[0].Code);
    }

    [Fact]
    public void MouseButtonsAreReleasedToo()
    {
        var tracker = new HeldKeyTracker();
        tracker.Record(InputEvent.MouseButton(1, 0, up: false));
        tracker.Record(InputEvent.MouseButton(1, 2, up: false));

        Span<InputEvent> releases = new InputEvent[tracker.MaxReleaseEvents];
        int count = tracker.BuildReleaseAll(1, releases);

        Assert.Equal(2, count);
        Assert.All(releases[..count].ToArray(), e => Assert.Equal(InputEventKind.MouseButton, e.Kind));
        Assert.All(releases[..count].ToArray(), e => Assert.True(e.IsUp));
    }

    [Fact]
    public void MotionAndWheelCarryNoStateToRelease()
    {
        var tracker = new HeldKeyTracker();
        tracker.Record(InputEvent.MouseMove(1, 10, 10));
        tracker.Record(InputEvent.MouseWheel(1, 120, horizontal: false));

        Assert.Equal(0, tracker.HeldCount);
    }

    [Fact]
    public void RepeatedKeyDownCountsOnce()
    {
        // Auto-repeat sends key-down many times without an intervening key-up.
        var tracker = new HeldKeyTracker();
        for (int i = 0; i < 50; i++)
        {
            tracker.Record(KeyDown(ScanA));
        }

        Assert.Equal(1, tracker.HeldCount);
    }

    [Fact]
    public void TrackersAreIndependentPerSourcePeer()
    {
        // Two peers may drive one machine at once; releasing one peer's Shift must not clear the
        // other's, or the second peer's typing silently loses its modifier.
        var fromPeerA = new HeldKeyTracker();
        var fromPeerB = new HeldKeyTracker();

        fromPeerA.Record(KeyDown(ScanLeftShift));
        fromPeerB.Record(KeyDown(ScanLeftShift));

        Span<InputEvent> releases = new InputEvent[fromPeerA.MaxReleaseEvents];
        fromPeerA.BuildReleaseAll(1, releases);

        Assert.Equal(0, fromPeerA.HeldCount);
        Assert.Equal(1, fromPeerB.HeldCount);
    }

    [Fact]
    public void ClearForgetsWithoutEmittingReleases()
    {
        var tracker = new HeldKeyTracker();
        tracker.Record(KeyDown(ScanA));
        tracker.Clear();

        Span<InputEvent> releases = new InputEvent[tracker.MaxReleaseEvents];
        Assert.Equal(0, tracker.BuildReleaseAll(1, releases));
    }

    [Fact]
    public void RecordingAllocatesNothing()
    {
        var tracker = new HeldKeyTracker();
        tracker.Record(KeyDown(ScanA));

        long before = GC.GetAllocatedBytesForCurrentThread();
        for (int i = 0; i < 10_000; i++)
        {
            tracker.Record(KeyDown(ScanA));
            tracker.Record(KeyUp(ScanA));
            tracker.Record(InputEvent.MouseMove(i, 1, 1));
        }

        Assert.Equal(0, GC.GetAllocatedBytesForCurrentThread() - before);
    }
}
