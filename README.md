# Virtual KM Switch

Drive several Windows machines from one keyboard and mouse, over the LAN. Input is captured on the
machine you are sitting at and replayed natively on the target, so games and relative-motion
applications behave as though the hardware were plugged in — and because the injecting process runs
as SYSTEM on its own desktop, **you can answer a UAC prompt on a remote machine.**

Pair two machines once with a code. Then press `Ctrl+Alt+1` and your keyboard and mouse are driving
the other machine; press `Ctrl+Alt+0` and they come back.

There is no server and nothing shared between machines beyond the moment of pairing. Each machine
keeps its own list of the machines it trusts and decides for itself which one, if any, it is driving.

For how and why it is built this way, see [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Layout

| | |
|---|---|
| `src/rust/` | The service, the session agent and everything they share: a Cargo workspace, its crates in `crates/`. |
| `src/csharp/` | The controller UI, .NET with Avalonia (`VirtualKmSwitch.Gui`), and its solution, `VirtualKmSwitch.slnx`. |
| `deploy/service.ps1` | Installs and manages the service from a development build. |
| `installer/` | Builds the setup (an MSI inside a Burn bundle) that installs everything. |

---

## Status

| | State |
|---|---|
| Pairing by code, trust, mesh, discovery | Works; tested end to end over real TLS |
| Engage / disengage from hotkeys and the controller, stuck-key release | Works; tested end to end |
| Capture, blocking, hotkeys, panic key, hook watchdog | Ported from the C# version |
| Calibrated injection, including into a live UAC prompt | Ported from the C# version |
| Controller: status, machines, pairing, hotkey editor, tray icon | Works |
| Clipboard sync | Not implemented |

249 Rust tests pass: `cargo test --workspace`, in `src/rust`.

---

## Requirements

- Windows 10 1809 (build 17763) or later, x64
- Rust 1.80 or later (`rustup`), MSVC toolchain
- For the controller: .NET SDK 10.0.400 or later. Avalonia comes from NuGet; no workload is needed.
- Administrator rights to install the service, and to pair or unpair machines. **Everyday use of the
  controller never needs elevation**, and it is never run elevated: when pairing needs administrator
  rights it asks for them with a UAC prompt, for that one change.

---

## Build

Rust commands run in `src/rust` and .NET commands in `src/csharp`; the PowerShell scripts run from the
repository root.

```bash
cargo build --release --workspace
```

```bash
dotnet build VirtualKmSwitch.slnx
```

---

## Install and run

### With the installer

```bash
powershell -ExecutionPolicy Bypass -File installer\build.ps1
```

This release-builds the service and agent, publishes the controller self-contained, and writes
`target\installer\VirtualKmSwitch-<version>-x64-setup.exe`, with the MSI it wraps beside it. WiX comes
from NuGet; nothing else needs installing. The version is the workspace's, from `src/rust/Cargo.toml`.

Setup installs to `C:\Program Files\Virtual KM Switch`, registers and starts the service as LocalSystem,
adds inbound firewall rules for it on private and domain networks, and puts **Virtual KM Switch** in the
Start menu. The target machine needs no .NET runtime. Running a newer build's setup replaces the old
one in place, closing a running controller and starting it again afterwards, with no reboot. Uninstall
from *Installed apps*; pairings and the machine's certificate in `%ProgramData%\VirtualKmSwitch` are kept,
so reinstalling picks them back up.

Use the setup rather than the MSI on its own. Setup stops the service before the MSI runs; an MSI
upgrade with the service running finds the session agents holding files it cannot close, so it asks
for a reboot and leaves the old controller running (ARCHITECTURE.md §12).

If the service was installed with `deploy\service.ps1`, run `service.ps1 uninstall` first: both
register a service named `VirtualKmSwitch`.

### By hand, from a development build

#### 1. The service

Must run as LocalSystem. Nothing else can create objects in the `Global` namespace, place a process
inside another session, or touch the Winlogon desktop — and all three are required for the agents to
work at all.

From an **elevated** PowerShell prompt, after `cargo build --release` in `src/rust`:

```bash
powershell -ExecutionPolicy Bypass -File deploy\service.ps1 install
```

This registers `src\rust\target\release\vkms-service.exe`, adds an inbound firewall rule for it on private and
domain networks, and starts it. Other actions: `start`, `stop`, `status`, `uninstall`, and `logs`.

A healthy install shows two agents — one on the `Default` desktop, one on `Winlogon`:

```bash
powershell -ExecutionPolicy Bypass -File deploy\service.ps1 status
```

#### 2. The controller

Unelevated, and optional — the machine remains a fully functioning peer with nobody signed in.

```bash
dotnet run --project VirtualKmSwitch.Gui
```

- **Status** — is the service healthy, are both agents up, how many events have flowed. Also a
  *Return control to this machine* button, the panic hotkey's visible equivalent.
- **Machines** — show a pairing code, enter one, see and remove paired machines.
- **Hotkeys** — bind `Ctrl+Alt+1..9` to paired machines. These are local to this machine.

Closing the window **hides it to the notification area**. Left-click the tray icon to bring it back,
or right-click for Open / Exit.

#### 3. Development mode

To run the service in the foreground without installing it:

```bash
cargo run -p vkms-service -- --console --no-agents --data-dir target\dev-data --pipe VirtualKmSwitch.Dev
```

`--data-dir` and `--pipe` keep a development run from touching an installed service's state or pipe.
Unelevated, the control pipe and pairing work but **agent launch fails** — creating `Global\` shared
memory needs `SeCreateGlobalPrivilege`, which only LocalSystem holds. The log says so explicitly.

---

## Pairing two machines

1. On the first machine, **Machines → Show a pairing code.** A code appears — four groups of four —
   with a five-minute countdown.
2. On the second machine, **Machines → Enter a code**, and type it. Case, spaces and dashes do not
   matter, and the alphabet has `I`, `O`, `0` and `1` removed, so reading it aloud works.
3. The second machine finds the first by its announcement on the network. Where multicast is blocked,
   it offers to connect by name or address instead.
4. Both machines now list each other, the link comes up, and each gives the other its first free
   hotkey.

The code works once. Pairing closes on success, on its timeout, or after ten failed attempts.

Pairing A with B and A with C does not pair B with C. Removing a machine on one side stops that side
trusting it; remove it on the other side too to undo the pairing completely.

Pairing and removing require administrator rights, because a paired machine can type into this one.
The controller asks with a UAC prompt when you show a code, enter one, or remove a machine; cancelling
a code you are showing needs no prompt.

**Upgrading from the C# version:** the previous cluster membership (`roster.json`) is not carried
over, and each machine has a new certificate, so machines must be paired again. Hotkey bindings and
the machine's identity in `local.json` are kept.

---

## Hotkeys

| Hotkey | Action |
|---|---|
| `Ctrl+Alt+1` … `Ctrl+Alt+9` | Drive the machine bound to that slot |
| `Ctrl+Alt+0` | Return control to this machine |
| `Ctrl+Alt+Shift+Esc` | **Panic** — release every key, unblock, return control |

Slots 1–9 are yours to assign. The other two are fixed: losing the way back matters more than any
convenience gained by rebinding it.

If a machine is removed, any hotkey pointing at it becomes **unassigned** rather than being reused.

---

## Files it writes

| Path | Contents |
|---|---|
| `%ProgramData%\VirtualKmSwitch\peers.json` (+ `.bak`) | The machines this one is paired with. |
| `%ProgramData%\VirtualKmSwitch\local.json` | This machine's node id and hotkey bindings. |
| `%ProgramData%\VirtualKmSwitch\node.crt`, `node.key` | This machine's TLS certificate, and its private key encrypted with DPAPI. |
| `%ProgramData%\VirtualKmSwitch\service.log`, `agent.*.log` | Logs. |

### Network

| Port | Purpose |
|---|---|
| UDP `239.255.42.99:42099` | Multicast discovery |
| TCP `42100` | Mesh connections between paired machines (mutual TLS 1.3) |
| TCP `42101` | Pairing, only while a pairing code is showing |

---

## Development

```bash
cargo test --workspace
```

### Verification harnesses

They answer questions a build cannot.

```bash
# Hook liveness: installs hooks, removes them the way Windows does on a timeout, and checks the
# watchdog notices and recovers. Moves the cursor by a net zero.
cargo run -p vkms-spike -- --hooks

# Calibrated mouse replay: injects and then measures the cursor, so the correction is demonstrated
# rather than asserted. Add --keyboard for the scancode check (needs a focused Notepad).
cargo run -p vkms-spike

# Hotkeys, blocking, capture and panic, by hand. Nothing is sent anywhere: captured events are
# printed, so you see exactly what would have gone to the service.
cargo run -p vkms-spike -- --engage

# Control pipe: runtime verbs succeed, administrative ones are refused unless elevated.
cargo run -p vkms-spike -- --control [--pipe <name>]
```

The secure-desktop check needs only this machine. Run it from a **normal** PowerShell after a release
build, with the service stopped. It asks you to approve one prompt, which starts a SYSTEM helper that
places the real agent on the Winlogon desktop the way the service does; it then raises a second
prompt, which the agent should dismiss with Escape within about a second. Nothing is elevated, and
nothing stays registered.

```bash
powershell -ExecutionPolicy Bypass -File src\rust\spike\run-uac-check.ps1
```

---

## Troubleshooting

**The controller says "Not running".** The service is not installed or not started.
`deploy\service.ps1 status`, or `Get-Service VirtualKmSwitch` for an MSI install.

**Status says "Running, but not all agents are up".** A missing `Winlogon` agent means UAC prompts on
this machine cannot be answered remotely. Check `%ProgramData%\VirtualKmSwitch\agent.Winlogon.log`.

**Entering a code says no machine is showing it.** The network may block multicast. Choose to connect
by address when offered, and check the firewall rule allows TCP 42101 on the machine showing the code.

**Paired, but "not connected".** Check TCP 42100 is reachable between the two machines. The machine
with the lower node id dials the other.

**Your antivirus flagged it.** Expected, and not a false positive in spirit: injecting into the secure
desktop is exactly the capability malware wants.

**UAC prompts still cannot be driven.** Some hardening and policy configurations block secure-desktop
injection. The fallback is `PromptOnSecureDesktop=0`.

**Remote mouse movement travels the wrong distance.** Calibration re-probes a few seconds after
pointer settings change, and only while nobody is driving from this machine.

---

## Security notes

- The session agent runs as **SYSTEM**, reads every keystroke, and can inject input on the secure
  desktop. Shared-memory rings are ACL'd to SYSTEM only, because a writable input ring is an
  injection primitive.
- The control pipe is reachable by any authenticated local user **by design**, but every request is
  authorised from the caller's own token. Changing trust requires Administrators; unclassified verbs
  default to requiring Administrators.
- Machines authenticate by **pinned certificate thumbprint**, not by a CA. The pairing code is used
  exactly once, and the proof is bound to both certificates — so a leaked code cannot impersonate a
  paired machine, and an interceptor presenting its own certificate cannot produce a matching proof.
- The service's data directory is restricted so ordinary users cannot plant a `peers.json`.

---

## Non-goals

Video or display switching, file transfer, drag-and-drop between machines, non-Windows peers,
gamepad forwarding, and driving another machine's BIOS or pre-boot environment.

---

## AI disclosure

AI was used to assist in the development of this project. If you are not comfortable with that, that
is understandable, and you are free not to use it.

---

## License

GNU General Public License v3.0; see [LICENSE](LICENSE). The installer's theme files,
`installer/bundle/Theme.xml` and `Theme.wxl`, are adapted from WiX and stay under the Microsoft
Reciprocal License, as their headers say.
