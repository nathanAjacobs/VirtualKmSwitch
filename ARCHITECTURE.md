# Virtual KM Switch — architecture

One keyboard and mouse, driving any paired Windows machine on the LAN. Input is captured on the
machine you are sitting at and replayed natively on the target, so games and relative-motion
applications behave as though the hardware were plugged in.

This document explains how it is built and, more usefully, *why* — most of the design is a response
to a Windows behaviour that is either undocumented or documented misleadingly. Where something was
measured rather than assumed, the measurement is here, because several of these findings contradict
what the documentation implies.

The service and session agent are Rust (`src/rust/crates/`). The controller UI is .NET with
Avalonia (`src/csharp/VirtualKmSwitch.Gui`) and talks to the service only over the control pipe.

---

## 1. The shape of the thing

Every machine runs the same software and is a peer. There is no server, no coordinator, and no
state shared over the network.

```
   Machine A                 Machine B                 Machine C
   driving C ───────────────────────────────────────►  local
                             driving C ─────────────►   ▲
                                                    (both drive C;
   Machine D                                         C's own keyboard
   local ◄── driven by nobody                        still works)
```

Two things exist between machines, and only two:

1. **Pairing.** Two machines become able to talk by a one-time setup: one shows a code, the other
   has it typed in. Each then records the other's certificate in its *own* list. Pairing A with B
   and A with C does not introduce B to C.
2. **Input.** A machine that is driving another sends it input over the link pairing made possible.

Everything else is local to each machine:

- **Which machine it is driving, if any.** Each machine decides for itself where its own keyboard
  and mouse go. Nobody arbitrates, so two people can drive the same machine at once.
- **Its hotkey map.** `Ctrl+Alt+1` means whatever *this* machine says it means.
- **Whom it trusts.** Removing a machine here stops this machine trusting it; the other machine keeps
  its own list until it is removed there too.

Each machine is independently in one of two states:

- **Local** — an entirely ordinary machine. Local input works, hooks pass everything through, and it
  accepts injected input from any paired machine that is driving it.
- **Driving(target)** — local input is swallowed by the hooks and forwarded to `target` instead.

Because targeting is per machine, A can be driving C while D is driving A. That only works if
injected events can be told apart from physical ones with certainty — see
[loopback suppression](#5-loopback-suppression), which is load-bearing rather than a nicety.

### Why there is no coordinator

An earlier design elected a host to propagate a replicated roster (a CRDT) and to grant exclusive
leases on targets. It was removed. With trust established pairwise and engagement decided locally,
there is nothing left to coordinate: no shared list to converge, no lease to grant, and no election
to run when a machine sleeps. Every failure mode those mechanisms existed to handle — a host
vanishing, a partition healing, a stale lease — no longer has anything to happen to.

---

## 2. Process model

Three processes per machine, with a strict split of privilege.

| Component | Runs as | Responsibility |
|---|---|---|
| **`VirtualKmSwitch.Gui`** (Avalonia, C#) | Interactive user, **unelevated** | Pure controller: status, paired machines, pairing, hotkey configuration, tray. No input, no network, never on the latency path. |
| **`vkms-service`** (Rust) | LocalSystem, session 0, auto-start | TLS mesh, pairing, discovery, routing, which machine this one is driving. Spawns and supervises the agents. Serves the control pipe. |
| **`vkms-agent`** (Rust) | **SYSTEM, one instance per desktop** | Everything input: raw-input capture, low-level hooks and blocking, hotkey matching, `SendInput` injection — for the desktop it was created on. |

### Why the input code cannot live in the service

Windows desktops are isolation boundaries. Hooks are **per-desktop**, and `SendInput` reaches only
the desktop its calling thread is attached to. A UAC prompt runs on the separate `Winlogon` secure
desktop, which no `Default`-desktop process can touch regardless of elevation. And services live in
**session 0**, which is non-interactive and has no access to the interactive session's window
station — so **a service cannot call `SendInput` at all**, however privileged it is.

The documented crossing is to spawn a helper *into* the interactive session. This is verified
working: a session-0 service placed a SYSTEM agent in the console session and that agent dismissed a
live UAC prompt.

### Why the agent must be SYSTEM, not the logged-in user

The user has no access to the `Winlogon` desktop. The service duplicates its own SYSTEM token, sets
`TokenSessionId` to the active console session via `SetTokenInformation`, and launches with that
(`src/rust/crates/vkms-win/src/session.rs`).

This also solves a problem that would otherwise be invisible: **at medium integrity the low-level
hook is not called at all while a higher-integrity window has focus.** Keystrokes would leak to the
local machine at exactly the moment you believed you were driving a remote one — silently, with
nothing in the return value or `GetLastError` to reveal it. A SYSTEM agent is above all of that.

### One agent per desktop — MEASURED, and the first design was wrong

The plan assumed one agent could cover every desktop by moving a thread with `SetThreadDesktop`.
**Measured, that does not work.** Two SYSTEM agents injecting at the same instant, on the same
secure desktop:

| Agent | How it reached Winlogon | `SendInput` result |
|---|---|---|
| Created on `Winsta0\Default` | `SetThreadDesktop` | **0/2, error 5 (ACCESS_DENIED)** |
| Created on `Winsta0\Winlogon` | created there | **2/2, error 0 — prompt dismissed** |

So **a process may only inject on the desktop it was created on**, and the restriction is symmetric.
`SetThreadDesktop` succeeds and is useless for this purpose.

Consequently the service spawns an agent on `Winsta0\Default` *and* one on `Winsta0\Winlogon`, both
at startup, and each agent gates itself on whether its own desktop currently owns input. Because
both are already running, a UAC prompt appearing costs no process startup.

### Following the console between sessions

Agents belong to whichever session owns the physical console. On a logon, a logoff or a fast user
switch the service kills both agents and starts a fresh pair in the new session. It learns of the
change from the SCM's session-change notifications as they happen, and also checks every two seconds
in case one is missed; the notification only prompts a look, and the decision is made by comparing
`WTSGetActiveConsoleSessionId` with the session the agents are in.

**A change of console session also ends driving.** Which machine this one is driving is held by the
service, not the agents, so without this it would survive the switch: the next person at the console
— even at the logon screen — would find their keyboard blocked and forwarded to a machine they never
chose. Disengaging also sends that machine a release for everything held, which the outgoing agents
cannot do, since they are terminated rather than asked to stop.

`--console` runs receive no SCM notifications and rely on the two-second check alone.

### A settle delay is required after a desktop switch

Injecting 5 ms after the desktop became `Winlogon` reported `accepted 2/2, error 0` and **the prompt
did not move**. The identical call at +750 ms dismissed it. The desktop switches before consent.exe
has created and focused its dialog, so input delivered in that window is silently discarded.

This is the sharpest available demonstration that **`SendInput` returning success is not evidence of
delivery** — it reported success in both cases.

### What this buys

- UAC prompts on a remote machine are drivable — the headline requirement.
- The controller needs no elevation for anything but changing trust, and even then is not itself
  elevated: it asks for a UAC prompt for that one request (below).
- A machine can be driven at its logon screen, before anyone signs in.
- Raw-input registration is safe: it is per-process last-call-wins, so keeping capture out of the
  UI process removes any chance of the UI stack silently stealing the registration.

### Honest caveats

- Secure-desktop injection is exactly the capability malware wants, so endpoint security products
  may flag it. Expect to allow-list during development.
- It depends on OS hardening and policy, and Microsoft could tighten it. The fallback is
  `PromptOnSecureDesktop=0`, which renders UAC on the normal desktop and needs no special handling.
- A SYSTEM process that reads every keystroke and injects input is a serious privilege boundary. Its
  IPC surface is the attack surface, and is ACL'd accordingly.

---

## 3. Capture and blocking — MEASURED

The design assumed raw input and low-level hooks were independent pipelines. Microsoft documents
this nowhere. Measured on real hardware, **the answer differs per channel**:

| Channel | Blocked in hook? | `WM_INPUT` still fires? | Capture source |
|---|---|---|---|
| Mouse motion | yes | **yes** (877 hook / 867 raw) | **Raw input** — true pre-ballistics deltas |
| Mouse buttons + wheel | yes | **yes** (12 / 12) | **Raw input** — same ordered stream |
| Keyboard | yes | **no** (92 / 0) | **Hook** — `KBDLLHOOKSTRUCT` carries scancode + E0 |

Consequences:

- **Mouse keeps full relative fidelity.** Motion is blocked *and* captured raw, so no `ClipCursor`
  pinning and no hook-derived deltas are needed.
- **Keyboard capture is the hook**, which costs nothing: the hook struct already carries the scancode
  and extended flag. It is in fact the *better* source, because `vkCode` disambiguates the
  NumLock/Pause `0x45` scancode collision directly, instead of needing an E1-prefix state machine.
- **All mouse capture stays on one ordered stream**, so a drag's button-down / move / button-up
  interleaving is preserved with no merging logic.
- **The same leak reaches local games.** Any application registered for raw mouse input still gets
  `WM_INPUT` while we block, and a game that acts on it whenever its window is active keeps moving
  locally. So while engaged the agent gives the foreground to a **focus decoy**
  (`vkms-win/src/focus_decoy.rs`): a zero-alpha, click-through tool window with no taskbar button
  and no Alt+Tab entry. It is created on engage, and on release it hands the foreground back to
  whatever held it before, then is destroyed.
- **The decoy must not live in the process that owns the hooks — MEASURED.** While a window
  belonging to the hooking process holds the foreground, Windows does not call that process's
  low-level keyboard hook at all: no chord was ever seen, and `Ctrl+Alt+Shift+Esc` reached the
  desktop as `Alt+Esc`. A decoy inside the agent therefore disabled every hotkey, panic included, for
  exactly as long as input was blocked. So each agent starts a helper copy of itself
  (`vkms-agent --focus-decoy`) that owns the window, and tells it show or hide over its standard
  input. When that input closes, because the agent exits or is killed, the helper hands the
  foreground back and exits too.
- Both sources stamp the performance counter at capture. It is used rather than Rust's `Instant`
  because the stamp has to mean the same thing in the agent that takes it and the service that
  converts it, and `Instant` keeps its origin private.

### Replay on the target

- Keyboard: `wVk = 0`, `wScan = MakeCode`,
  `dwFlags = KEYEVENTF_SCANCODE | (E0 ? EXTENDEDKEY) | (BREAK ? KEYUP)`. Scancodes rather than
  virtual keys, so differing layouts do not matter.
- **Fake shift must be filtered.** With Shift or NumLock active the driver injects synthetic shift
  events around the grey navigation keys. They arrive as `0x2A`/`0x36` **with E0 set**, whereas real
  LShift is `0x2A` *without* E0 — so the test is exact.
- **Pause/Break cannot be replayed by scancode.** It emits `e1 1d 45 e1 9d c5` and **nothing on
  release**. It is transmitted as a virtual-key event instead.
- `MakeCode == 0xFF` (keyboard overrun sentinel) is dropped.
- Wheel `usButtonData` is a **signed** short and is sign-extended.
- `RAWMOUSE.usFlags & MOUSE_MOVE_ABSOLUTE` is branched on: tablets, RDP sessions and most VMs report
  absolute positions, and assuming deltas makes the cursor teleport.

---

## 4. Mouse fidelity — MEASURED

Relative injection is what games and RDP-style applications expect. But `MOUSEINPUT` is subject to
mouse speed and two threshold values, and *"it is thus possible for the system to multiply specified
relative mouse movement … by up to four times"* — on top of the nonlinear "Enhance pointer
precision" curve.

Calibration on the target, measured rather than modelled (`src/rust/crates/vkms-core/src/calibration.rs`):

1. Read `SPI_GETMOUSE` (acceleration flag and both thresholds) and `SPI_GETMOUSESPEED` (1–20).
2. **Split each delta into sub-threshold chunks** so the 2×/4× doubling never triggers, submitted as
   one `SendInput` array rather than N calls.
3. Apply the inverse of the speed multiplier (1–20 maps to 1/32× … 3.5×, not linearly).
4. **Apply an empirically measured correction curve.**

Steps 1–3 alone are not sufficient. On the test machine (thresholds 6/10, acceleration on, speed 10):

| Requested | Uncalibrated | Chunked only | Chunked + measured curve |
|---|---|---|---|
| 50 px | 87 | 47 | **50** |
| 200 px | 387 | 180 | **199** |
| 400 px | 786 | — | **413** |

Chunking removes the doubling but leaves ~10% residual from the smooth curve the `MOUSEINPUT`
documentation does not describe, and **the residual is magnitude-dependent**, so a single scalar
cannot correct it. The agent's probe (`src/rust/crates/vkms-agent/src/probe.rs`) measures actual cursor travel
at several magnitudes and builds an interpolated correction curve. It moves the cursor for well under
a second and restores it.

With **"Enhance pointer precision" off** there is no curve to dodge, so chunking switches off.

**Settings are monitored, not read once.** The agent polls every few seconds and re-probes on change.
It polls rather than handling `WM_SETTINGCHANGE` because **broadcast messages are not delivered to
message-only windows**, which is what the input threads use.

`SPI_SETMOUSE` / `SPI_SETMOUSESPEED` are never called: that would mutate the user's settings visibly
and leak on crash.

---

## 5. Loopback suppression

Every machine both injects and hooks, and a machine can be driven while it drives another, so both
paths must ignore our own injected events. Without this the mesh forms an input feedback loop.

The mechanism is a magic `dwExtraInfo` signature (`0x564B_4D53`, "VKMS") stamped on everything we
inject. The hook checks `LLMHF_INJECTED`/`LLKHF_INJECTED` plus that signature and **passes those
through** rather than blocking. Raw input echoes the same value back in `RAWMOUSE.ulExtraInformation`
/ `RAWKEYBOARD.ExtraInformation`, so the identical test works there.

**Filtering raw input on `RAWINPUTHEADER.hDevice == 0` instead is wrong, and was measured to be
wrong:** physical mice legitimately report a null device handle (3979 of 4094 samples on the test
machine), so that filter silently discarded *all* real mouse motion while looking like a Windows
behaviour rather than a bug.

---

## 6. Inside the agent

### Threads

`LowLevelMouseProc` is explicit: the call is made by **sending a message** to the installing thread,
and *"if the hook procedure times out … the hook is silently removed without being called. There is
no way for the application to know whether the hook is removed."* A thread pumping both hooks and
`WM_INPUT` at 1–8 kHz can blow `LowLevelHooksTimeout` — and Windows then quietly unhooks us. The user
keeps typing believing they are driving a remote box while keystrokes land locally. So:

| Thread | Owns | Rules |
|---|---|---|
| **Hooks** | Own message-only window; both LL hooks. Above normal. | Read an atomic engaged flag, check the injected signature, match the hotkey chord, return. **No allocation, no blocking, no I/O.** |
| **RawInput** | Own message-only window; raw-input registration. | `GetRawInputData` for the current event, then drain with `GetRawInputBuffer` into a reused buffer. |
| **Injection** | Drains the inbound ring and calls `SendInput`; applies the service's engaged state. | Creates no window or hook. |
| **DesktopGate** | Polls which desktop owns input. | Reads the name only; never attaches. |
| **HookHealth** | The watchdog. | Probes only while this desktop is showing. |

`GetMessage` removes the current `WM_INPUT` before returning, so `GetRawInputBuffer` only sees events
that arrived *after* it — hence handling the current event first, then draining. Buffered entries are
packed on 8-byte boundaries, so the drain advances by the rounded-up entry size.

In Rust the hook and window procedures are plain `extern "system"` functions, so the delegate-lifetime
traps the C# version had to code around (a collected thunk, a finaliser unhooking on another thread)
do not exist. What remains is thread affinity: each procedure finds its state through a thread-local,
and hooks are installed, reinstalled and removed only on the thread that owns them.

### The engaged decision is the service's

The agent matches hotkeys but does not act on them. `Ctrl+Alt+1` means "the machine in slot 1 of
this machine's map", and only the service knows the map and whether that machine is reachable. So
the agent reports the chord to the service through the capture ring (as an event of its own kind,
which keeps it ordered with the keystrokes around it), the service decides, and the service writes
the result into a control word in the shared ring header. The agent's injection thread applies it —
blocking local input and releasing locally held modifiers, or flushing releases on the way out.

It is applied on *change*, not continuously. A local stand-down — the panic chord, or a capture ring
the service has stopped draining — must hold until the service says something new, not be
overridden a moment later by a stale "engaged" from a service that may be wedged.

**Panic acts first and reports afterwards.** It is the way out when something in that loop has gone
wrong, so the agent unblocks and releases immediately, and then tells the service.

### The hook watchdog, and why its result is tri-state

Windows offers no API to ask whether a hook is installed, so liveness is proven by injecting a probe
and confirming the hook observed it. **The result must be tri-state**: an agent can only inject while
its own desktop is showing, so on the inactive agent the probe is refused — indistinguishable from a
dead hook unless "could not deliver" is reported separately from "not observed". Treating it as death
reinstalled healthy hooks every five seconds. Found in the first live service run.

`vkms-spike --hooks` proves the watchdog end to end: it removes the hooks the way Windows does on a
timeout, confirms the probe reports them dead, reinstalls, and confirms they are alive again.

### Hotkeys are matched in the hook

`RegisterHotKey` is unusable here: low-level hooks run *before* hotkey processing, so once we swallow
keys it never fires.

| Hotkey | Action |
|---|---|
| `Ctrl+Alt+1..9` | Drive the machine bound to slot N in **this machine's own** map |
| `Ctrl+Alt+0` | Return control to this machine |
| `Ctrl+Alt+Shift+Esc` | **Panic:** release all keys, unblock, return control |

The panic hotkey deliberately avoids `Pause`, the pathological E1 key with no key-up.

### Stuck-key safety

The most common KVM bug. Held keys and buttons are tracked **on both ends**:

- The **sender** tracks what it has forwarded and flushes releases when it stops driving.
- The **receiver** tracks what each *source machine* has pressed — per source, because several
  machines may drive one at once, and releasing one machine's Shift must not clear another's — and
  releases it when that source sends `ReleaseAll` or disconnects.

The receiver's tracking is the one that cannot be missed: the sender's flush can race the switch
that caused it or find the link already gone, but the receiver always knows what it was told to
press. Switching straight from one machine to another releases everything on the one being left —
the chord that caused the switch had its modifiers held down on it.

---

## 7. IPC

Two channels, because they have opposite requirements.

### Control — a named pipe (controller ↔ service)

Low rate; needs structure and authentication. Message-framed (4-byte little-endian length + JSON,
PascalCase keys, enums as strings), capped at 64 KB. The Rust server is
`src/rust/crates/vkms-ipc/src/pipe_server.rs`; the C# client is `src/csharp/VirtualKmSwitch.Gui/Ipc/`.

**It is two-tier, and that is what lets the controller stay unelevated:**

- *Runtime* — engage, disengage, read status, read the paired machines, edit this machine's hotkeys,
  close a pairing window early (which can only narrow who may pair).
- *Administrative* — show a pairing code, pair with a code, remove a paired machine. Anything that
  widens or changes whom this machine trusts.

Verbs **default to Administrative**: a verb added later without being classified is locked down
rather than silently exposed, and a verb the service does not recognise is refused with a message
rather than a dropped connection. Authorisation is decided by **impersonating the connection** and
reading the caller's own token. The client connects at *identification* level: the service may read
who is calling, but may not act as them. Failure to read the token fails closed.

The pipe is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a pipe someone else created first under
the same name makes the service fail at startup instead of leaving the controller talking to an
impostor. Authenticated users get read and write but *not* `FILE_CREATE_PIPE_INSTANCE`, so they
cannot add an instance of their own and wait for the controller to connect to it.

**An administrative request from the unelevated controller** is sent as the user first. Only if the
service refuses it for want of rights does the controller start a copy of its own executable
through the UAC prompt (`--elevated-request`, `Services/Elevation.cs`). That copy has no window. It
takes the one request over a private pipe with an unguessable name, sends it to the service with its
elevated token, writes back the answer and exits. The private pipe admits Administrators only. An
elevated process has that group enabled and an unelevated one does not, so no other program running
as the user can stand in for the helper. The request travels over that pipe rather than the command
line because a typed pairing code is a secret. The helper carries only the administrative verbs.

### Input hot path — shared memory (agent ↔ service)

A named file mapping plus a named auto-reset event, in both directions, laid out as an SPSC ring with
producer and consumer indices on separate cache lines and a header recording magic, capacity and slot
size — checked on open, because two sides disagreeing about the layout would read the wrong memory
rather than fail. Segments are ACL'd to SYSTEM only: a writable input ring is an injection primitive.

One writer *process* per ring, but several writer *threads*: the agent's hook and raw-input threads
both capture. Each end is serialised in-process by an uncontended lock. (The C# version wrote from
both threads without serialising them; that is fixed, and tested.)

The service's router waits on the agents' events rather than a timer — Windows' default timer
resolution is 15.6 ms, several times the whole latency budget.

---

## 8. Transport

- **Full mesh between paired machines**, so input needs no relaying. Duplicate dials are avoided by
  *lower node id dials higher*.
- **Mutual TLS 1.3** (rustls), self-signed ECDSA P-256 certificates generated on first run, trusted
  only by pinned SHA-256 thumbprint. The verifiers still check the handshake signature — that proves
  the peer holds the key — but the decision to trust a certificate is the pin store's alone.
- **The private key is stored encrypted with DPAPI** under the service's account (LocalSystem), so
  only code already running as SYSTEM can recover it. The certificate beside it is public.
- **A hello exchange follows the handshake, and is not optional.** TLS 1.3 completes the *client's*
  handshake before the server has validated the client certificate, so a peer whose certificate the
  far side rejects still reaches an authenticated state locally. Only a completed round trip proves
  the link works in both directions.
- **Every timestamp on the wire is microseconds, never counter ticks.** Tick frequency is a property
  of the machine, so two peers can scale the same count differently — and the failure is silent. The
  conversion is a divide by a precomputed `f64`, because `ticks * 1_000_000 / frequency` overflows
  after about eleven days of uptime at 10 MHz.
- TCP rather than UDP: a key-up must never overtake its key-down.
- **Framing:** 2-byte length + 1-byte type + payload. Input frames are hand-encoded (twelve bytes per
  event); control messages are JSON in the same envelope.
- **Clock synchronisation is part of the transport.** Heartbeats carry NTP's four timestamps, and
  the sample with the **lowest round trip** is kept, since its one-way delays were most nearly equal.
  The responder's own thinking time is subtracted, and a held sample expires after 30 s because
  crystal drift would otherwise swamp a budget measured in microseconds.
- **Discovery:** UDP multicast beacon on `239.255.42.99:42099`, carrying node id, machine name, mesh
  port and — only while a pairing window is open — the pairing port and a hash of the code. Mesh
  connections are accepted on TCP `42100`. Beacons are unauthenticated and only ever produce an
  address to try; the certificate decides. Each machine also remembers where its paired machines
  were last reached, so a network that drops multicast still reconnects after a restart.

Nothing listens until there is a reason to: the mesh port is bound only while this machine has paired
machines or a pairing is in progress, and the beacon only then.

### Pairing

1. On machine A, **Show a pairing code** opens a pairing window: a listener of its own on TCP
   `42101`, for five minutes, behind a fresh code — four groups of four from a 32-character alphabet
   without `I`, `O`, `0`, `1` (80 bits).
2. On machine B, the code is typed in. B finds A by the beacon advertising
   `SHA256("vkms-pairing-v1:" + code)[..8]`, or at an address typed in where multicast is blocked.
3. B connects over TLS (certificate not yet pinned) and both sides prove knowledge of the code with
   `HMAC(code, "vkms-enroll-v1" ‖ local-thumbprint ‖ remote-thumbprint ‖ nonceA ‖ nonceB)`.
   **Binding the proof to both certificate thumbprints is what stops a machine-in-the-middle.**
4. A sends a final acknowledgement, so B never records a pairing A refused. Each side records the
   other's node id, thumbprint, name and mesh address in its own `peers.json`, and gives it the next
   free hotkey so it works without configuration.
5. **The code is never used again.** Links authenticate by pinned certificate alone, so a leaked code
   cannot impersonate a paired machine.

**The pairing window is a listener of its own, not a flag on the mesh listener.** A socket that is
not bound cannot be reached by a mistake. The window closes on its timeout, on the first success, or
after ten attempts — closing rather than throttling, because a run of failures against an 80-bit code
is not somebody mistyping.

The code is 80 bits rather than something shorter because the beacon advertises a hash of it and an
accepted handshake reveals an HMAC over it, so anyone on the network can attempt to recover it
offline while the window is open. At 80 bits that is not a race they can win.

**A machine-in-the-middle is refused *and named*.** Each side reports which certificate it observed
from the other. That field is untrusted and used only to choose the error message: if the proof
fails and the peer saw a certificate this machine never presented, the user is told the connection
was not direct rather than that the code was wrong. Verified with a test that terminates TLS on both
sides with its own certificate and relays the plaintext verbatim.

**Pins are a projection of `peers.json`, never a file of their own.** A separate pin store would be
a second record of the same fact, and the two would eventually disagree — at which point the machine
either refuses a peer it considers paired, or accepts one it has removed. Removing a machine also
drops its open link, since revoking the pin only stops new ones.

---

## 9. Local state

| File | Contents |
|---|---|
| `%ProgramData%\VirtualKmSwitch\peers.json` (+ `.bak`) | The machines this one is paired with: node id, certificate thumbprint, name, last address. |
| `%ProgramData%\VirtualKmSwitch\local.json` | This machine's node id and hotkey bindings. |
| `%ProgramData%\VirtualKmSwitch\node.crt`, `node.key` | This machine's certificate, and its private key encrypted with DPAPI. |
| `%ProgramData%\VirtualKmSwitch\*.log` | Service and per-desktop agent logs. |

When the service creates the directory it replaces the inherited DACL: SYSTEM and Administrators may
write, users may read. ProgramData otherwise lets ordinary users create files in new subdirectories,
and a user who could drop a `peers.json` there first would decide whose keystrokes this machine
accepts.

**Atomic writes:** `peers.json` is written to a temporary file, then swapped in with `ReplaceFile`,
retaining a `.bak`. The first save copies the primary to the backup explicitly. A corrupt primary
falls back to `.bak`; failure of both is an explicit "pair again" state surfaced in the UI, never a
silent empty list, because an empty list and an unreadable one look identical on screen. Unknown
fields round-trip rather than being dropped.

**Hotkey bindings name a machine by node id, never a slot index**, so a binding keeps pointing at the
right machine as machines come and go. If a machine is removed, its slot is left **empty rather than
reused**: pressing a familiar key and typing into an unexpected machine is the worst outcome available.

---

## 10. Performance engineering

Added latency is what makes a software KVM feel broken, so it is budgeted and measured.

**Budget**, physical event → injected on target, excluding wire time: **≤ 2 ms p99**. The hook
callback is budgeted separately and far harder, since it must never approach
`LowLevelHooksTimeout`.

### Allocation-free hot path

Every input event from capture to wire allocates nothing: events are `Copy` values stored inline in
rings; the SPSC ring, held-key tracker, hotkey matcher, codec, coalescer and latency histogram are
fixed-size; send and receive buffers are per connection and reused. Each test binary that covers the
hot path installs a counting global allocator and asserts that steady-state traffic allocates
**exactly zero** times.

The in-process SPSC ring is split into `Producer` and `Consumer` handles that each need `&mut` to move
data, so a call site with several producers has to put the producer behind a lock *visibly* — the
single-producer rule is enforced by the compiler rather than a comment.

### Coalescing and the network

**Coalesce opportunistically and never delay deliberately**: drain everything currently queued, merge
consecutive mouse moves into one accumulated delta, send one batched write. At idle each event goes
immediately; under load batching emerges from the drain. **Merging applies only to consecutive mouse
moves**: a button or key event terminates the merge, so a click can never be reordered relative to
the motion it followed.

One TLS record per drain, not per event. Each link has a `PeerSendPump`: a bounded SPSC queue and a
Tokio task that drains it and does the framed write. The router never waits on the network: it
writes into the queue and signals a `tokio::sync::Notify`, whose stored permit makes a wake that
arrives before the task waits impossible to lose. Neither enqueueing nor waking the task allocates.

The pump was originally a dedicated OS thread per link that spun briefly and then parked, calling
`block_on` for each write because the connection itself was already async. Replacing it with a task
was measured on the 8 kHz loopback test, 35 runs of each build: the direct (offset-free) median rose
from 35 µs to 44 µs, the cost of losing the spin and waking through the scheduler, while p99
(95 → 88 µs) and p999 (151 → 136 µs) moved by less than their run-to-run spread. Both are a small
fraction of the 2 ms budget. Pump tasks run at the runtime's normal thread priority, where the
thread raised its own; that difference would show only under CPU contention, which the loopback
test does not create.

### Measured, at 8 kHz on loopback (release build)

| | Rust | C# predecessor |
|---|---|---|
| p50 added latency | **70–140 µs** | 120 µs |
| p99 | **~290 µs** | 880 µs |
| Clock offset between ends | within ±10 µs | −2 µs |
| Events dropped / reordered | 0 / 0 | 0 / 0 |

Two findings from taking the original measurement still shape the test:

- **The first heartbeat on a new link must be sent after the receive loop has started**, and as a
  burst of three: a lone cold ping once reported 2.4 ms on a path that actually cost 98 µs, and since
  the lowest round trip wins, that one sample governed every figure for the link.
- **An unpaced test measures the harness, not the transport.** The load test paces to 8 kHz and
  cross-checks the reported percentile against a direct measurement from the batch timestamp.

### Backpressure

**Mouse moves may be merged or dropped under pressure; key and button events are never dropped** — a
lost key-up leaves a modifier stuck on a remote machine. If a queue fills with events that may not be
dropped, the machine stops driving that peer rather than silently corrupting state. A send that cannot
complete within two seconds is treated as a lost link for the same reason.

---

## 11. Project map

| Crate / project | Purpose |
|---|---|
| `src/rust/crates/vkms-core` | `InputEvent`, the monotonic clock, the SPSC ring, held-key tracking, hotkey matching, keyboard translation, mouse calibration. Pure and testable without a keyboard. |
| `src/rust/crates/vkms-win` | Every Win32 call, behind safe wrappers: message windows, hooks, raw input, injection, desktops, session launch, shared memory, events, SDDL, DPAPI, `ReplaceFile`, logging. |
| `src/rust/crates/vkms-ipc` | The control pipe protocol, server and client; the shared-memory ring and agent channel. |
| `src/rust/crates/vkms-net` | Framing, clock sync, latency, coalescing, the send pump, pins, the TLS identity and verifiers, peer connections, the mesh, discovery, pairing. |
| `src/rust/crates/vkms-service` | The service: peer registry, local settings, router, remote-input release, mesh host and pairing, control endpoint, agent supervisor. |
| `src/rust/crates/vkms-agent` | The session agent: engagement, hook watchdog, calibration probe, desktop gating. |
| `src/rust/crates/vkms-spike` | Verification harnesses for what a build cannot prove. |
| `src/csharp/VirtualKmSwitch.Gui` | The Avalonia controller. Talks to the service only over the control pipe; its only native code is the tray icon and its restart registration (CsWin32). |

---

## 12. The tray icon — MEASURED

The controller's only native dependency. `Shell_NotifyIcon(NIM_ADD)` and `NIM_DELETE` were both
confirmed against the real notification area, and close-to-tray was driven end to end.

- **The tray window is a hidden top-level window, not message-only.** A message-only window cannot be
  brought to the foreground, and `TrackPopupMenuEx` on an owner that is not foreground never receives
  the click that should dismiss the menu, so it stays on screen. The `SetForegroundWindow` / posted
  `WM_NULL` pair around `TrackPopupMenuEx` are not optional.
- **`TaskbarCreated` is handled.** Explorer restarting destroys every tray icon without notifying its
  owner.
- **Only a person's close hides; Windows' close exits.** Restart Manager closes applications with
  `WM_QUERYENDSESSION` / `ENDSESSION_CLOSEAPP`, which Avalonia turns into an application shutdown;
  hiding then vetoed it. Measured against `RmShutdown` on the installed controller: unforced it
  failed (351) and the controller kept running, forced it waited out the 30 s hang timeout and
  killed it. Now an unforced `RmShutdown` takes ~150 ms, and `RegisterApplicationRestart` brings
  the controller back, with `--background` (no window) if it was only in the tray. One controller
  per session (a `Local\` named event): a second launch shows the first and exits, since an
  upgrade's restart and the installer's finish page would otherwise both start one.
- **Upgrades go through a Burn bundle that stops the service first.** With the service running, an
  MSI upgrade still closed nothing: Restart Manager decides at `InstallValidate`, before
  `StopServices`, and the agents (SYSTEM, in the user's session) are processes it cannot close, so
  it logged "a critical application holds file[s] in use", closed no application at all and asked for
  a reboot, while the old controller ran on from a file moved into `C:\Config.Msi`. Nothing in an MSI
  runs elevated that early: immediate custom actions run with the user's unelevated token. The
  bundle (`installer/bundle`) runs `vkms-service --stop` elevated first, which returns once the SCM
  reports the service stopped, and that comes only after the agents have ended. Measured with the
  service, both agents and the controller running: setup exits 0 with no restart, the controller
  comes back as a new unelevated process (still `--background`), and setup over setup removes the
  old bundle without touching the service. Uninstall is left as it was (not measured): the stop
  cannot run on uninstall without also running when an upgrade removes the old bundle, so an
  uninstall with the service running meets the agents as the bare MSI did.

---

## 13. Current state

**Built and tested** (`cargo test --workspace`: 249 tests, including two complete services pairing
by code and driving each other over real TLS on loopback):

- Pairing by code, both directions of trust, refusal of a wrong code and of an intercepted connection.
- Engage and disengage from the control pipe and from the hotkeys, with release on the machine being
  left; input both ways at once; unpairing dropping the link, the pin and the hotkey.
- The mesh, discovery, clock sync, coalescing and backpressure; the 8 kHz latency budget.
- The control pipe with both privilege tiers, over a real named pipe.
- On real hardware: hook install, silent-unhook detection and reinstall (`vkms-spike --hooks`).

**Re-verified on real hardware since the port:**

- Calibrated injection (`vkms-spike`: 50 → 49, 200 → 200, 400 → 411 px, with the settings in §4).
- The SYSTEM agent on the Winlogon desktop dismissing a live UAC prompt (`src/rust/spike/run-uac-check.ps1`).
  The secure desktop showed for 806 ms: the 750 ms settle delay, then injection. The script stands
  in for the service and feeds the real agent Escape, so this covers the agent's launch, desktop
  gate, settle delay and injection, but not input arriving from a paired machine.
- Keyboard capture while driving, from the hotkeys (`vkms-spike --engage`): engage, return and
  panic all act, and leaving releases the held modifiers and sends `ReleaseAll`. Mouse buttons were
  not exercised in that run.

Clipboard sharing is not implemented.

---

## 14. Non-goals

Video/display switching, file transfer, drag-and-drop between machines, non-Windows peers, gamepad
forwarding, and driving another machine's BIOS or pre-boot environment.
