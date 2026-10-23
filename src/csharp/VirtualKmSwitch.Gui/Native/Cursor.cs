using Windows.Win32;

namespace VirtualKmSwitch.Gui.Native;

/// <summary>Reads and moves the cursor. Used to measure what injected motion actually produced.</summary>
public static class Cursor
{
    public static (int X, int Y) GetPosition()
    {
        if (PInvoke.GetCursorPos(out System.Drawing.Point point))
        {
            return (point.X, point.Y);
        }

        return (0, 0);
    }

    public static bool SetPosition(int x, int y) => PInvoke.SetCursorPos(x, y);
}
