using VirtualKmSwitch.Core;
using VirtualKmSwitch.InputEngine;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class KeyboardTranslatorTests
{
    private const uint Down = KeyboardTranslator.WM_KEYDOWN;
    private const uint Up = KeyboardTranslator.WM_KEYUP;
    private const uint SysDown = KeyboardTranslator.WM_SYSKEYDOWN;
    private const uint SysUp = KeyboardTranslator.WM_SYSKEYUP;
    private const uint Extended = KeyboardTranslator.LLKHF_EXTENDED;

    [Theory]
    [InlineData(Down, false)]
    [InlineData(SysDown, false)]
    [InlineData(Up, true)]
    [InlineData(SysUp, true)]
    public void KeyTransitionDirectionIsTaggedFromTheMessage(uint message, bool expectUp)
    {
        Assert.True(KeyboardTranslator.TryTranslate(message, 0x41, 0x1E, 0, 100, out InputEvent e));
        Assert.Equal(expectUp, e.IsUp);
        Assert.Equal(0x1E, e.Code);
        Assert.Equal(100, e.Timestamp);
    }

    [Fact]
    public void ExtendedKeysAreFlaggedRatherThanPrefixed()
    {
        // Right Ctrl: same scancode as left, distinguished only by the E0 prefix. The prefix must
        // travel as a flag, never packed into the scancode itself.
        Assert.True(KeyboardTranslator.TryTranslate(Down, 0xA3, 0x1D, Extended, 1, out InputEvent e));
        Assert.Equal(0x1D, e.Code);
        Assert.True((e.Flags & InputEventFlags.Extended) != 0);
    }

    [Theory]
    [InlineData(0x2A)] // left shift
    [InlineData(0x36)] // right shift
    public void FakeShiftIsDropped(ushort scanCode)
    {
        // The driver brackets grey navigation keys with synthetic shifts when Shift or NumLock is
        // active. Forwarding them leaves shift stuck or double-toggled on the target.
        Assert.False(KeyboardTranslator.TryTranslate(Down, 0xA0, scanCode, Extended, 1, out _));
    }

    [Theory]
    [InlineData(0x2A)]
    [InlineData(0x36)]
    public void RealShiftIsKept(ushort scanCode)
    {
        // A genuine shift never carries the extended flag, which is what makes the filter exact.
        Assert.True(KeyboardTranslator.TryTranslate(Down, 0xA0, scanCode, 0, 1, out InputEvent e));
        Assert.Equal(scanCode, e.Code);
    }

    [Fact]
    public void PauseIsSentAsVirtualKeyBecauseItsScancodeCannotBeReplayed()
    {
        // Pause emits E1 1D 45 and reports no release at all, so scancode replay cannot reproduce
        // it. It travels as a virtual key and the target synthesises the key-up.
        Assert.True(KeyboardTranslator.TryTranslate(Down, 0x13, 0x45, 0, 1, out InputEvent e));
        Assert.True((e.Flags & InputEventFlags.VirtualKey) != 0);
        Assert.Equal(0x13, e.Code);
    }

    [Fact]
    public void CtrlBreakIsSentAsVirtualKey()
    {
        Assert.True(KeyboardTranslator.TryTranslate(Down, 0x03, 0x46, Extended, 1, out InputEvent e));
        Assert.True((e.Flags & InputEventFlags.VirtualKey) != 0);
        Assert.Equal(0x03, e.Code);
    }

    [Fact]
    public void NumLockIsNotConfusedWithPause()
    {
        // Both report scancode 0x45. Keying on the scancode alone would emit a spurious NumLock
        // toggle on every Pause press; the virtual key is what separates them.
        Assert.True(KeyboardTranslator.TryTranslate(Down, 0x90, 0x45, 0, 1, out InputEvent numLock));
        Assert.False((numLock.Flags & InputEventFlags.VirtualKey) != 0);
        Assert.Equal(0x45, numLock.Code);

        Assert.True(KeyboardTranslator.TryTranslate(Down, 0x13, 0x45, 0, 1, out InputEvent pause));
        Assert.True((pause.Flags & InputEventFlags.VirtualKey) != 0);
    }

    [Fact]
    public void KeyboardOverrunSentinelIsDropped()
    {
        Assert.False(KeyboardTranslator.TryTranslate(Down, 0xFF, 0xFF, 0, 1, out _));
    }

    [Fact]
    public void NonKeyboardMessagesAreIgnored()
    {
        Assert.False(KeyboardTranslator.TryTranslate(0x0200 /* WM_MOUSEMOVE */, 0, 0, 0, 1, out _));
    }

    [Fact]
    public void TranslationAllocatesNothing()
    {
        // Runs on the hook thread, which is on a hard latency budget: a GC pause there risks
        // overrunning LowLevelHooksTimeout, after which Windows unhooks us silently.
        KeyboardTranslator.TryTranslate(Down, 0x41, 0x1E, 0, 1, out _);

        long before = GC.GetAllocatedBytesForCurrentThread();
        for (int i = 0; i < 10_000; i++)
        {
            KeyboardTranslator.TryTranslate(Down, 0x41, 0x1E, 0, i, out _);
        }

        Assert.Equal(0, GC.GetAllocatedBytesForCurrentThread() - before);
    }
}
