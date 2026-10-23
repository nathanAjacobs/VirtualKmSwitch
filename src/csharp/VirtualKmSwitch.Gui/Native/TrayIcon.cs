using System.ComponentModel;
using Windows.Win32;
using Windows.Win32.Foundation;
using Windows.Win32.UI.Shell;
using Windows.Win32.UI.WindowsAndMessaging;

namespace VirtualKmSwitch.Gui.Native;

/// <summary>
/// One entry in the tray icon's context menu. A <see cref="CommandId"/> of zero is a separator.
/// </summary>
public sealed record TrayMenuItem(uint CommandId, string Text)
{
    public static TrayMenuItem Separator { get; } = new(0, string.Empty);

    public bool IsSeparator => CommandId == 0;
}

/// <summary>
/// A notification-area icon, owning the hidden window that receives its callbacks.
///
/// The window is a real top-level window rather than a message-only one. That is not an
/// oversight: a message-only window cannot be brought to the foreground, and a popup menu whose
/// owner is not the foreground window never receives the click that should dismiss it, so it
/// stays on screen over the desktop. That is why SetForegroundWindow before and a posted WM_NULL
/// after are both here.
///
/// Everything runs on the window's own thread, so the caller's thread -- the UI thread, in the
/// controller -- is never blocked by the modal menu.
/// </summary>
public sealed class TrayIcon : IDisposable
{
    /// <summary>The message the shell sends us for mouse activity on the icon.</summary>
    private const uint CallbackMessage = PInvoke.WM_APP + 0x100;

    /// <summary>Tooltips are capped at 128 characters including the terminator.</summary>
    private const int MaxTooltipLength = 127;

    private readonly uint _iconId = 1;
    private readonly IReadOnlyList<TrayMenuItem> _menuItems;
    private readonly MessageWindow _window;

    /// <summary>
    /// Explorer restarting destroys every tray icon without telling the owner, and broadcasts
    /// this registered message once the new taskbar is ready. Ignoring it is why so many
    /// applications lose their tray icon for the rest of the session.
    /// </summary>
    private readonly uint _taskbarCreated;

    private HICON _icon;
    private bool _ownsIcon;
    private string _tooltip;
    private volatile bool _added;
    private bool _disposed;

    public TrayIcon(string tooltip, IReadOnlyList<TrayMenuItem>? menuItems = null)
    {
        _tooltip = tooltip ?? string.Empty;
        _menuItems = menuItems ?? [];
        _taskbarCreated = RegisterTaskbarCreatedMessage();
        _icon = LoadApplicationIcon(out _ownsIcon);

        _window = new MessageWindow("Tray", HandleMessage, kind: MessageWindowKind.HiddenTopLevel);

        if (!TryNotify(NOTIFY_ICON_MESSAGE.NIM_ADD))
        {
            _window.Dispose();
            ReleaseIcon();
            throw new InvalidOperationException("Shell_NotifyIcon(NIM_ADD) failed.", new Win32Exception());
        }

        _added = true;
    }

    /// <summary>Raised when the user left-clicks or double-clicks the icon.</summary>
    public event Action? Activated;

    /// <summary>Raised with the command id of the menu entry the user chose.</summary>
    public event Action<uint>? CommandInvoked;

    /// <summary>Updates the hover text, e.g. to show whether the service is reachable.</summary>
    public void SetTooltip(string tooltip)
    {
        _tooltip = tooltip ?? string.Empty;

        if (_added)
        {
            TryNotify(NOTIFY_ICON_MESSAGE.NIM_MODIFY);
        }
    }

    private bool HandleMessage(uint message, nuint wParam, nint lParam, out nint result)
    {
        result = 0;

        // Explorer came back, so the icon we added is gone even though nothing told us.
        if (message == _taskbarCreated && _taskbarCreated != 0)
        {
            TryNotify(NOTIFY_ICON_MESSAGE.NIM_ADD);
            return true;
        }

        if (message != CallbackMessage)
        {
            return false;
        }

        // With the default (version 0) semantics the mouse message arrives in lParam and the
        // icon id in wParam.
        switch ((uint)lParam)
        {
            case PInvoke.WM_LBUTTONUP:
            case PInvoke.WM_LBUTTONDBLCLK:
                Activated?.Invoke();
                return true;

            case PInvoke.WM_RBUTTONUP:
                ShowContextMenu();
                return true;

            default:
                return true;
        }
    }

    private unsafe void ShowContextMenu()
    {
        if (_menuItems.Count == 0)
        {
            return;
        }

        HMENU menu = PInvoke.CreatePopupMenu();
        if (menu.IsNull)
        {
            return;
        }

        try
        {
            foreach (TrayMenuItem item in _menuItems)
            {
                if (item.IsSeparator)
                {
                    PInvoke.AppendMenu(menu, MENU_ITEM_FLAGS.MF_SEPARATOR, 0, default(PCWSTR));
                    continue;
                }

                fixed (char* text = item.Text)
                {
                    PInvoke.AppendMenu(menu, MENU_ITEM_FLAGS.MF_STRING, item.CommandId, new PCWSTR(text));
                }
            }

            (int x, int y) = Cursor.GetPosition();
            var owner = new HWND(_window.Handle);

            // Required before the menu appears, for the reason given on the class.
            PInvoke.SetForegroundWindow(owner);

            uint flags = (uint)(TRACK_POPUP_MENU_FLAGS.TPM_RIGHTBUTTON
                | TRACK_POPUP_MENU_FLAGS.TPM_RETURNCMD
                | TRACK_POPUP_MENU_FLAGS.TPM_NONOTIFY);

            // TPM_RETURNCMD hands the selection back directly, so there is no WM_COMMAND to route
            // and no chance of a stray command arriving after the menu has gone.
            BOOL selection = PInvoke.TrackPopupMenuEx(menu, flags, x, y, owner, null);

            // The other half of the foreground dance: without this the next click on the icon can
            // be swallowed.
            PInvoke.PostMessage(owner, PInvoke.WM_NULL, default, default);

            if (selection.Value != 0)
            {
                CommandInvoked?.Invoke((uint)selection.Value);
            }
        }
        finally
        {
            PInvoke.DestroyMenu(menu);
        }
    }

    private unsafe bool TryNotify(NOTIFY_ICON_MESSAGE command)
    {
        var data = new NOTIFYICONDATAW
        {
            cbSize = (uint)sizeof(NOTIFYICONDATAW),
            hWnd = new HWND(_window.Handle),
            uID = _iconId,
            uFlags = NOTIFY_ICON_DATA_FLAGS.NIF_MESSAGE
                | NOTIFY_ICON_DATA_FLAGS.NIF_ICON
                | NOTIFY_ICON_DATA_FLAGS.NIF_TIP,
            uCallbackMessage = CallbackMessage,
            hIcon = _icon,
        };

        // The inline array copy throws rather than truncating, so the caller's string is cut to
        // fit here instead.
        data.szTip = _tooltip.Length > MaxTooltipLength ? _tooltip[..MaxTooltipLength] : _tooltip;

        return PInvoke.Shell_NotifyIcon(command, data);
    }

    /// <summary>
    /// Prefers this executable's own icon, falling back to the generic application icon.
    /// </summary>
    /// <remarks>
    /// A tray icon that silently fails to appear is much worse than a generic-looking one, so the
    /// fallback is unconditional rather than an error.
    /// </remarks>
    private static unsafe HICON LoadApplicationIcon(out bool ownsIcon)
    {
        ownsIcon = false;

        string? executable = Environment.ProcessPath;
        if (!string.IsNullOrEmpty(executable))
        {
            HICON small = default;
            fixed (char* path = executable)
            {
                if (PInvoke.ExtractIconEx(new PCWSTR(path), 0, null, &small, 1) > 0 && !small.IsNull)
                {
                    ownsIcon = true;
                    return small;
                }
            }
        }

        // A shared system icon: not ours to destroy, hence ownsIcon stays false.
        return PInvoke.LoadIcon(default(HINSTANCE), PInvoke.IDI_APPLICATION);
    }

    private void ReleaseIcon()
    {
        if (_ownsIcon && !_icon.IsNull)
        {
            PInvoke.DestroyIcon(_icon);
        }

        _icon = default;
        _ownsIcon = false;
    }

    private static unsafe uint RegisterTaskbarCreatedMessage()
    {
        fixed (char* name = "TaskbarCreated")
        {
            return PInvoke.RegisterWindowMessage(new PCWSTR(name));
        }
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;

        if (_added)
        {
            // Removed explicitly: an abandoned icon lingers in the tray until the user happens to
            // hover over it, which looks like the application is still running.
            TryNotify(NOTIFY_ICON_MESSAGE.NIM_DELETE);
            _added = false;
        }

        _window.Dispose();
        ReleaseIcon();
    }
}
