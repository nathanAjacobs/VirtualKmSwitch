using VirtualKmSwitch.InputEngine;
using Xunit;

namespace VirtualKmSwitch.Tests;

public class HotkeyMatcherTests
{
    private const ushort LCtrl = 0xA2;
    private const ushort RCtrl = 0xA3;
    private const ushort LAlt = 0xA4;
    private const ushort LShift = 0xA0;
    private const ushort Key1 = 0x31;
    private const ushort Key0 = 0x30;
    private const ushort Home = 0x24;
    private const ushort Escape = 0x1B;
    private const ushort KeyA = 0x41;

    private static HotkeyMatch Press(HotkeyMatcher matcher, ushort key, out bool swallow) =>
        matcher.Process(key, isUp: false, out swallow);

    private static void Hold(HotkeyMatcher matcher, params ushort[] keys)
    {
        foreach (ushort key in keys)
        {
            matcher.Process(key, isUp: false, out _);
        }
    }

    [Fact]
    public void CtrlAltDigitEngagesTheMatchingSlot()
    {
        var matcher = new HotkeyMatcher();
        Hold(matcher, LCtrl, LAlt);

        HotkeyMatch match = Press(matcher, Key1, out bool swallow);

        Assert.Equal(HotkeyActionKind.Engage, match.Kind);
        Assert.Equal(1, match.Slot);
        Assert.True(swallow, "the trigger key must not reach the local machine");
    }

    [Fact]
    public void CtrlAltZeroReturnsControlLocally()
    {
        var matcher = new HotkeyMatcher();
        Hold(matcher, LCtrl, LAlt);

        Assert.Equal(HotkeyActionKind.ReturnLocal, Press(matcher, Key0, out _).Kind);
    }

    [Fact]
    public void CtrlAltHomeClaimsHost()
    {
        var matcher = new HotkeyMatcher();
        Hold(matcher, LCtrl, LAlt);

        Assert.Equal(HotkeyActionKind.ClaimHost, Press(matcher, Home, out _).Kind);
    }

    [Fact]
    public void PanicRequiresShiftAsWell()
    {
        var matcher = new HotkeyMatcher();

        // Ctrl+Alt+Esc is not the panic chord and must not fire it.
        Hold(matcher, LCtrl, LAlt);
        Assert.False(Press(matcher, Escape, out _).IsMatch);

        Hold(matcher, LShift);
        Assert.Equal(HotkeyActionKind.Panic, Press(matcher, Escape, out _).Kind);
    }

    [Fact]
    public void EitherSideOfAModifierPairCounts()
    {
        var matcher = new HotkeyMatcher();
        Hold(matcher, RCtrl, LAlt);

        Assert.Equal(HotkeyActionKind.Engage, Press(matcher, Key1, out _).Kind);
    }

    [Fact]
    public void ReleasingOneOfAPairKeepsTheModifierHeld()
    {
        // Holding both controls and releasing one must not clear Control, or a chord typed with
        // both hands would silently stop matching.
        var matcher = new HotkeyMatcher();
        Hold(matcher, LCtrl, RCtrl, LAlt);

        matcher.Process(LCtrl, isUp: true, out _);

        Assert.True(matcher.CurrentModifiers.HasFlag(Modifiers.Control));
        Assert.Equal(HotkeyActionKind.Engage, Press(matcher, Key1, out _).Kind);
    }

    [Fact]
    public void ExtraModifiersPreventAMatch()
    {
        // Ctrl+Alt+Shift+1 is not Ctrl+Alt+1. Matching it would hijack chords belonging to other
        // applications.
        var matcher = new HotkeyMatcher();
        Hold(matcher, LCtrl, LAlt, LShift);

        Assert.False(Press(matcher, Key1, out bool swallow).IsMatch);
        Assert.False(swallow);
    }

    [Fact]
    public void PlainKeysAreNeitherMatchedNorSwallowed()
    {
        var matcher = new HotkeyMatcher();

        Assert.False(Press(matcher, KeyA, out bool swallow).IsMatch);
        Assert.False(swallow);

        Hold(matcher, LCtrl, LAlt);
        Assert.False(Press(matcher, KeyA, out swallow).IsMatch);
        Assert.False(swallow);
    }

    [Fact]
    public void ModifierKeysThemselvesPassThrough()
    {
        // Swallowing modifiers would break every ordinary shortcut on the machine while idle.
        var matcher = new HotkeyMatcher();

        Press(matcher, LCtrl, out bool swallow);
        Assert.False(swallow);

        Press(matcher, LAlt, out swallow);
        Assert.False(swallow);
    }

    [Fact]
    public void ReleaseOfASwallowedTriggerIsAlsoSwallowed()
    {
        // Otherwise the application underneath sees a key-up with no matching key-down, which some
        // apps treat as a stuck key.
        var matcher = new HotkeyMatcher();
        Hold(matcher, LCtrl, LAlt);
        Press(matcher, Key1, out _);

        matcher.Process(Key1, isUp: true, out bool swallow);
        Assert.True(swallow);

        // Only the one release is consumed, not every subsequent release of that key.
        matcher.Process(Key1, isUp: true, out swallow);
        Assert.False(swallow);
    }

    [Fact]
    public void ResetForgetsHeldModifiers()
    {
        var matcher = new HotkeyMatcher();
        Hold(matcher, LCtrl, LAlt);

        matcher.Reset();

        Assert.Equal(Modifiers.None, matcher.CurrentModifiers);
        Assert.False(Press(matcher, Key1, out _).IsMatch);
    }

    [Fact]
    public void MatchingAllocatesNothing()
    {
        // Runs inside the hook callback, where a GC pause risks the LowLevelHooksTimeout.
        var matcher = new HotkeyMatcher();
        Hold(matcher, LCtrl, LAlt);
        Press(matcher, Key1, out _);

        long before = GC.GetAllocatedBytesForCurrentThread();
        for (int i = 0; i < 10_000; i++)
        {
            matcher.Process(Key1, isUp: false, out _);
            matcher.Process(Key1, isUp: true, out _);
            matcher.Process(KeyA, isUp: false, out _);
        }

        Assert.Equal(0, GC.GetAllocatedBytesForCurrentThread() - before);
    }
}
