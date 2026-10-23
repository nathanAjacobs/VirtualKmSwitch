using System.ComponentModel;
using System.Runtime.InteropServices;
using Windows.Win32;
using Windows.Win32.Foundation;
using Windows.Win32.UI.WindowsAndMessaging;

namespace VirtualKmSwitch.Gui.Native;

/// <summary>
/// Handles one window message. Return <see langword="true"/> having set <paramref name="result"/>
/// to consume the message; return <see langword="false"/> for default processing.
/// </summary>
public delegate bool WindowMessageHandler(uint message, nuint wParam, nint lParam, out nint result);

/// <summary>What kind of window to create.</summary>
public enum MessageWindowKind
{
    /// <summary>
    /// HWND_MESSAGE. Invisible, unenumerable, and cheap — everything raw input and hooks need.
    /// </summary>
    MessageOnly,

    /// <summary>
    /// A real top-level window that is simply never shown.
    ///
    /// Needed only by the tray icon. A message-only window cannot be activated, and
    /// <c>TrackPopupMenuEx</c> depends on its owner being the foreground window to dismiss the
    /// menu when the user clicks away — on a message-only owner the menu is left stuck on screen
    /// with no way to close it.
    /// </summary>
    HiddenTopLevel,
}

/// <summary>
/// A window owning a dedicated thread and its own message pump.
///
/// In the controller its one use is the tray icon, which needs a window to receive the shell's
/// callbacks. Giving it a thread of its own means the modal tray menu never blocks the UI thread.
/// (The service's input threads use a Rust equivalent; see src/rust/crates/vkms-win/src/message_window.rs.)
/// </summary>
public sealed class MessageWindow : IDisposable
{
    /// <summary>Posted to ask the window to destroy itself on its owning thread.</summary>
    private const uint WM_APP_CLOSE = PInvoke.WM_APP + 1;

    /// <summary>
    /// Rooted for the process lifetime. Marshalling a delegate to native code does not root it,
    /// and a collected thunk crashes the moment Windows calls back — typically hours later.
    /// A static field is a GC root and is sufficient; GCHandle adds nothing here.
    /// </summary>
    private static readonly WNDPROC s_windowProcedure = WindowProcedure;

    /// <summary>
    /// The window procedure always runs on the thread that owns the window, so thread-local
    /// storage resolves the instance in a single field read — no dictionary lookup on a path
    /// that carries every WM_INPUT.
    /// </summary>
    [ThreadStatic]
    private static MessageWindow? t_current;

    private readonly WindowMessageHandler _handler;
    private readonly MessageWindowKind _kind;
    private readonly Thread _thread;
    private readonly TaskCompletionSource<bool> _ready = new(TaskCreationOptions.RunContinuationsAsynchronously);

    private HWND _hwnd;
    private ushort _classAtom;
    private volatile bool _disposed;

    public MessageWindow(
        string name,
        WindowMessageHandler handler,
        ThreadPriority priority = ThreadPriority.Normal,
        MessageWindowKind kind = MessageWindowKind.MessageOnly)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(name);
        _handler = handler ?? throw new ArgumentNullException(nameof(handler));
        _kind = kind;

        _thread = new Thread(() => ThreadBody(name))
        {
            Name = $"VirtualKmSwitch.{name}",
            IsBackground = true,
            Priority = priority,
        };
        _thread.SetApartmentState(ApartmentState.STA);
        _thread.Start();

        // Surface creation failures to the caller rather than leaving a dead thread behind.
        _ready.Task.GetAwaiter().GetResult();
    }

    /// <summary>The window handle, valid until <see cref="Dispose"/>.</summary>
    public unsafe nint Handle => (nint)_hwnd.Value;

    /// <summary>Posts a message to the window's own thread. Safe from any thread.</summary>
    public bool Post(uint message, nuint wParam = 0, nint lParam = 0)
    {
        HWND hwnd = _hwnd;
        return !hwnd.IsNull && PInvoke.PostMessage(hwnd, message, new WPARAM(wParam), new LPARAM(lParam));
    }

    private unsafe void ThreadBody(string name)
    {
        try
        {
            t_current = this;

            // A unique class name per instance: registration is process-wide, and two windows
            // sharing a name would collide across agent restarts within one process.
            string className = $"VirtualKmSwitch.{name}.{Environment.ProcessId}.{Environment.CurrentManagedThreadId}";

            fixed (char* classNamePtr = className)
            {
                var windowClass = new WNDCLASSEXW
                {
                    cbSize = (uint)Marshal.SizeOf<WNDCLASSEXW>(),
                    lpfnWndProc = s_windowProcedure,
                    lpszClassName = classNamePtr,
                };

                _classAtom = PInvoke.RegisterClassEx(windowClass);
                if (_classAtom == 0)
                {
                    throw new InvalidOperationException(
                        $"RegisterClassEx failed for '{className}'.", new Win32Exception());
                }

                // HWND_MESSAGE as the parent makes a window message-only: no display, no
                // z-order, no enumeration by other applications, but a real queue and a real
                // HWND, which is all that raw input registration and hooks require.
                bool messageOnly = _kind == MessageWindowKind.MessageOnly;

                _hwnd = PInvoke.CreateWindowEx(
                    // A top-level helper window is kept off the taskbar and out of Alt+Tab: it
                    // exists to own a tray icon, and showing it anywhere would be a bug the user
                    // could see.
                    dwExStyle: messageOnly ? 0 : WINDOW_EX_STYLE.WS_EX_TOOLWINDOW,
                    lpClassName: className,
                    lpWindowName: name,
                    // WS_VISIBLE is deliberately absent, so the window is never painted.
                    dwStyle: messageOnly ? 0 : WINDOW_STYLE.WS_POPUP,
                    X: 0, Y: 0, nWidth: 0, nHeight: 0,
                    hWndParent: messageOnly ? HWND.HWND_MESSAGE : default,
                    hMenu: default,
                    hInstance: default,
                    lpParam: null);

                if (_hwnd.IsNull)
                {
                    throw new InvalidOperationException("CreateWindowEx failed.", new Win32Exception());
                }
            }

            _ready.TrySetResult(true);
            PumpMessages();
        }
        catch (Exception ex)
        {
            _ready.TrySetException(ex);
        }
        finally
        {
            Cleanup();
            t_current = null;
        }
    }

    private unsafe void PumpMessages()
    {
        MSG message;
        while (true)
        {
            // GetMessage returns -1 on error, which must not be treated as a message.
            BOOL result = PInvoke.GetMessage(&message, default, 0, 0);
            if (result.Value <= 0)
            {
                break;
            }

            PInvoke.TranslateMessage(&message);
            PInvoke.DispatchMessage(&message);
        }
    }

    private static LRESULT WindowProcedure(HWND hwnd, uint message, WPARAM wParam, LPARAM lParam)
    {
        MessageWindow? instance = t_current;
        if (instance is not null)
        {
            if (message == WM_APP_CLOSE)
            {
                // DestroyWindow is thread-affine, so the request is posted here rather than
                // called from Dispose on the disposing thread.
                PInvoke.DestroyWindow(hwnd);
                return default;
            }

            if (message == PInvoke.WM_DESTROY)
            {
                PInvoke.PostQuitMessage(0);
                return default;
            }

            if (instance._handler(message, wParam.Value, lParam.Value, out nint handled))
            {
                return new LRESULT(handled);
            }
        }

        return PInvoke.DefWindowProc(hwnd, message, wParam, lParam);
    }

    private unsafe void Cleanup()
    {
        if (_classAtom != 0)
        {
            // MAKEINTATOM: an atom is passed where a string is expected by packing it into
            // the low word of the pointer.
            PInvoke.UnregisterClass(new PCWSTR((char*)(nint)_classAtom), default);
            _classAtom = 0;
        }

        _hwnd = default;
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;

        Post(WM_APP_CLOSE);

        if (!_thread.Join(TimeSpan.FromSeconds(5)))
        {
            // Background thread; the process is exiting anyway if this ever happens.
            return;
        }
    }
}
