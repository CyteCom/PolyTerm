# ARCHITECTURE.md

## 1. Shape of the system

One process. Three long-lived regions:

```
┌──────────────────────────────────────────────────────────────┐
│ Main thread — eframe/egui event loop                         │
│   Session tree · tile tree · tab groups · terminal/RDP panes │
│   Reads channel receivers, never blocks, renders             │
└───────────────▲──────────────────────────────┬───────────────┘
                │ frames, bytes, events        │ input, control
                │                              ▼
┌───────────────┴──────────────────────────────────────────────┐
│ tokio multi-threaded runtime (background)                    │
│   One task set per open session                              │
│   SSH · SFTP · serial · PTY · RDP                            │
└──────────────────────────────────────────────────────────────┘
                │
┌───────────────┴──────────────────────────────────────────────┐
│ Blocking pool — serialport reads, keyring, filesystem        │
│   Reached via spawn_blocking; never on the UI thread         │
└──────────────────────────────────────────────────────────────┘
```

The UI thread and the runtime share **no locks**. All communication is channels. This is
the single most important property in the design: it is what keeps a stalled SSH
connection or a slow RDP host from freezing the whole application.

## 2. Crate graph

```
                    polyterm-core
                   (traits, types)
                          ▲
      ┌──────────┬────────┼────────┬──────────┬──────────┐
      │          │        │        │          │          │
 polyterm-ssh  -serial  -pty    -rdp    polyterm-term  -store
      │          │        │        │          │          │
      └──────────┴────────┴────────┘          │          │
                   │                          │          │
                   │                    polyterm-ui ◄────┘
                   │                          │
                   └────────► apps/polyterm ◄─┘
```

`polyterm-core` is the only shared vocabulary. Backends implement its traits; the binary
calls `spawn` and hands the UI the resulting `TransportHandle` / `RdpHandle`, which are
concrete and carry no trace of which protocol produced them. The binary is the sole place
where a concrete backend type is named.

Neither trait is object-safe, and neither needs to be: `spawn` consumes `self`, and
`Transport` carries an associated `Config` type, so `Box<dyn Transport>` does not compile.
Erasure happens at the handle, not at the trait. The handles are already uniform, which is
what the UI actually needs — see §3.1.

The purpose of the layering is substitutability. Swapping `ironrdp` for FFI bindings to
`libfreerdp`, or `alacritty_terminal` for `wezterm-term`, must touch exactly one crate.

## 3. Core contracts

These live in `polyterm-core` and are the stable surface of the project. Treat changes to
them as design work, not implementation.

### 3.1 Transport

Every byte-stream session — SSH shell, serial port, local PTY — is the same shape.

```rust
/// A backend that opens byte-stream sessions of one kind. One value opens many sessions
/// and lives for the life of the process. Constructing it does no I/O.
pub trait Transport: Send + Sync + 'static {
    /// Per-session configuration. Holds no secrets — those arrive as prompts (§6).
    type Config: Send + 'static;

    /// Stable identifier for logs and the UI.
    const KIND: TransportKind;

    /// Start a session on `rt`. Returns once the session's tasks are spawned;
    /// progress is reported on the handle's `events`.
    fn spawn(&self, rt: &tokio::runtime::Handle, cfg: Self::Config)
        -> Result<TransportHandle, TransportError>;
}

/// The UI's end. `TransportHandle::new_pair()` builds it together with the backend's
/// mirror-image end, at the standard bounded capacities.
pub struct TransportHandle {
    pub output:  mpsc::Receiver<Bytes>,           // from the far end, already chunked
    pub input:   mpsc::Sender<Bytes>,             // keystrokes, pastes
    pub control: mpsc::Sender<ControlMsg>,        // out-of-band
    pub events:  mpsc::Receiver<TransportEvent>,  // None ⇒ backend gone, handle finished
}

pub enum ControlMsg {
    Resize { cols: u16, rows: u16 },  // SSH + PTY; no-op for serial
    Break,                            // serial BREAK; SSH break request
    SetSignal(SerialSignal, bool),    // DTR / RTS — serial only
    Reconnect,                        // after Disconnected; ignored where unsupported
    Disconnect,                       // answered with Disconnected, then events closes
}

pub enum TransportEvent {
    Connecting,
    HostKey(HostKeyPrompt),           // answer required — §6
    Credential(CredentialPrompt),     // answer required, may repeat — §6
    Authenticated,
    Connected,
    Disconnected { reason: DisconnectReason },  // a state, not an ending
    Error(TransportError),            // the session continues
}
```

Notes that matter:

- **Bounded channels, always.** An unbounded `output` channel turns a fast remote
  `cat` into unbounded memory growth. `new_pair()` builds every channel at the standard
  capacity and is the only way a backend should obtain them; the reader task feels the
  backpressure, and that is the design.
- **The runtime is passed in.** Sessions are opened from the UI thread, which is not a
  runtime thread. `Handle::current()` there is a panic at runtime; a `&Handle` parameter
  is a missing argument at compile time.
- **`Disconnected` does not finish the handle.** Only `events` closing does. A dropped
  link is a state the tab sits in and recovers from — automatically under FR-29, or on
  the user's `Reconnect` once an unplugged adapter returns under FR-49 — without the tab
  being torn down and rebuilt. `Error` is weaker still: the session continues.
- `Resize` being a no-op for serial is correct behaviour, not a gap.
- **Prompts are requests.** The task waits for an answer over a oneshot carried in the
  prompt. Never auto-accept in the transport layer; §6 says who answers.

### 3.2 RemoteDesktop

Deliberately parallel to `Transport`, but framebuffer-shaped rather than byte-shaped.
This is the substitution seam described in `SPIKE-RDP.md`.

```rust
pub trait RemoteDesktop: Send + Sync + 'static {
    fn spawn(&self, rt: &tokio::runtime::Handle, cfg: RdpConfig)
        -> Result<RdpHandle, RdpError>;
}

/// Built with `RdpHandle::new_pair()`, like its Transport counterpart.
pub struct RdpHandle {
    /// Damage-rect updates. Never a full-screen blit unless the server sent one.
    pub frames: mpsc::Receiver<FrameUpdate>,
    pub input:  mpsc::Sender<RdpInput>,
    pub events: mpsc::Receiver<RdpEvent>,      // None ⇒ finished
}

pub struct FrameUpdate {
    pub rect:   Rect,
    pub pixels: Arc<[u8]>,       // Arc so the UI can upload without copying
    pub stride: usize,
    pub format: PixelFormat,     // Bgra8 in practice; do not assume
}

pub enum RdpInput {
    Key { scancode: u16, down: bool, extended: bool },
    Mouse { x: u16, y: u16, buttons: MouseButtons, wheel: i16 },
    Clipboard(ClipboardData),
    Resize { width: u16, height: u16 },
}

pub enum RdpEvent {
    Connecting,
    Certificate(CertPrompt),          // answer required — §6
    Credential(CredentialPrompt),     // answer required before NLA — §6
    Connected { width: u16, height: u16 },
    ClipboardFromServer(ClipboardData),
    Disconnected { reason: DisconnectReason },
    Error(RdpError),
}
```

`RdpInput::Key` carries a **scancode**, not a character. RDP is scancode-based; going
through egui's translated character events will break non-US layouts and modifier
handling. Take raw keyboard input for RDP panes.

Lifecycle semantics are identical to `Transport`: `Disconnected` is a state, `Error`
means the session continues, and the handle is finished only when `events` closes.

### 3.3 Session model

```rust
pub struct SessionId(Uuid);

pub struct SessionSpec {
    pub id:       SessionId,
    pub name:     String,
    pub folder:   FolderPath,          // position in the session tree
    pub kind:     SessionKind,
}

pub enum SessionKind {
    Ssh(SshConfig),
    Serial(SerialConfig),
    LocalShell(PtyConfig),
    Rdp(RdpConfig),
}
```

`SessionSpec` is what gets persisted. It contains **no secrets** — only a
`CredentialRef` naming a keyring entry.

## 4. Terminal pipeline

```
TransportHandle.output ──► alacritty_terminal parser ──► Term<Listener> (grid state)
                                                              │
                                              damage tracking │
                                                              ▼
                                            renderable snapshot ──► egui texture
```

`polyterm-term` owns a `Term` and feeds it bytes. It performs **no I/O** — it is a pure
state machine over a byte stream, which makes it directly unit-testable: feed a byte
sequence, assert on the resulting grid.

Rendering constraints:

- Cursor blink and selection highlight are UI state, not terminal state.
- Scrollback lives in the `Term`, not in the UI.

**Glyph atlas — measured, not assumed (M2, 2026).** This section originally required a
bespoke glyph atlas and per-line damage-driven repaint, on the assumption that laying out
text per cell per frame would not survive a `cat`. Measurement retired that assumption.
`egui` already maintains its own font atlas and caches the galley for each
(glyph, font, colour); the per-cell `painter.text` path is therefore a cache hit plus a
shape push, not a re-layout. On a maximised 5120×1440 window — roughly 40,000 cells,
fully changed every frame while streaming a 10 MB file — building the whole frame's shapes
costs **~0.9 ms**, and the app holds a steady 60 fps with no dropped frames. NFR-5 wants
30 fps; the budget is 33 ms and we use under one.

So a custom atlas is **not** built: `egui`'s atlas is the shared atlas this section asked
for, and building our own would be premature optimisation. If a future case (a much faster
source, or a pathological grid) ever pushes the renderer, the fallback is a single
`Mesh` of textured quads over `egui`'s font-atlas UVs — but only when a measurement, taken
with the `POLYTERM_PERF` instrumentation in `polyterm-ui`, shows it is needed.

Immediate-mode rendering re-tessellates the whole frame regardless, so per-line damage
buys nothing on a repaint; damage's value is elsewhere — the UI repaints only when data
arrives (woken by `request_repaint`), not on a clock, so an idle terminal costs nothing.
`polyterm-term` still exposes damage in its snapshot for any consumer that can use it.

## 5. Rendering RDP

Each `FrameUpdate` is uploaded as a partial texture update into a persistent
`egui::TextureHandle` sized to the session's desktop dimensions. The pane draws that
texture scaled to the widget rect.

- Never allocate a new texture per frame.
- Coalesce: drain the whole `frames` receiver each UI frame and apply all pending
  updates before painting once.
- Scaling and DPI are a UI concern; the server is told a fixed resolution at connect
  time, and dynamic resize (if implemented) goes back as `RdpInput::Resize`.

## 6. Prompts that must reach the user

Host key acceptance, certificate acceptance, password entry, and key passphrase entry all
originate deep in a background task but must be answered by a human. The pattern is the
same for every one of them: the task sends a prompt on `events`, awaits a `oneshot`
carried inside the prompt, and continues. It never blocks a runtime thread waiting, and
it never defaults to accept.

```rust
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint: String,
    pub public_key: Vec<u8>,
    pub reply: oneshot::Sender<TrustDecision>,   // Reject | AcceptOnce | AcceptAndRemember
}

pub struct CredentialPrompt {
    pub request: CredentialRequest,              // Password | Passphrase | KeyboardInteractive
    pub credential: Option<CredentialRef>,       // where a stored answer would live
    pub reply: oneshot::Sender<CredentialReply>, // Cancelled | Secret { value, remember } | Responses
}
```

### 6.1 Who answers

A prompt carries only what the backend knows, and that is less than it looks. The SSH
crate can report the key a server presented; it cannot say whether that key is in the
known-hosts store, because the store is `polyterm-store` and backend crates depend on
`polyterm-core` alone. It can ask for a password; it has no keyring to look in.

So prompts are not answered by the UI directly. They are answered by the binary, which
has the store, the keyring, and the UI all in reach, and which decides how far each one
needs to travel:

```
backend ──HostKey──────► binary looks the key up in the store
                            Match    ──► reply AcceptOnce; the user never sees it
                            Unknown  ──► UI modal, KnownHostStatus::Unknown ──► reply
                            Changed  ──► UI modal, blocking warning (FR-23) ──► reply
                          AcceptAndRemember ──► the store records the key

backend ──Credential───► binary looks `credential` up in the keyring
                            hit      ──► reply Secret { remember: false }; no modal
                            miss     ──► UI modal ──► reply; `remember` ──► keyring writes it
                            KeyboardInteractive ──► always the UI; never remembered
```

This is why `known_host_status` is not a field of the prompt: the transport does not know
it. `KnownHostStatus` is what the binary attaches when a prompt has to go up to the UI.

### 6.2 Consequences

- **`SessionSpec` never needs a secret in it, and no backend ever needs keyring access.**
  The only path a password travels is keyring → binary → oneshot → backend, inside a
  `Secret<String>` that cannot be printed and is zeroised on drop.
- **Keyboard-interactive falls out for free.** The server drives an arbitrary sequence of
  prompts and each one is a `Credential` event. Nothing special-cases it.
- **RDP is the same shape.** NLA needs the password before CredSSP proceeds, so the RDP
  backend emits `Credential` early, and `Certificate` when TLS needs a decision.
- **`AcceptAndRemember` is a request to the answerer**, not something a backend does.
  Backends do not write to the known-hosts store or the keyring; they cannot.

## 7. Persistence

`polyterm-store` owns two things:

1. **Session tree.** SQLite via `rusqlite`, in the platform config directory
   (`directories::ProjectDirs`). SQLite over a JSON file because the tree will grow, it
   needs partial updates, and it must survive an unclean shutdown.
2. **Credentials.** The `keyring` crate, which maps to Windows Credential Manager and
   Secret Service / KWallet on Linux. The store holds only a `CredentialRef`.

We do not implement encryption ourselves. If the OS keyring is unavailable, the correct
behaviour is to prompt every time, not to fall back to a homegrown vault.

## 8. SFTP

`polyterm-ssh` exposes the SFTP client as a separate handle from the shell so the browser
pane and the terminal share one connection but operate independently. Directory listings
and transfers are async tasks; the pane renders from a cached listing and refreshes on
demand. Transfers report progress on their own channel.

## 9. Port forwarding

Local, remote, and dynamic (SOCKS) forwards are owned by the SSH session and outlive
individual shell tabs. They are listed and toggled from the session's properties, not
from a terminal tab. Closing the last shell tab must not silently tear down an active
tunnel — that is a confirmation prompt.

## 10. Tiling and input routing

### 10.1 The layout tree

The content area is a binary tree. Interior nodes are splits; leaves hold content.

```rust
enum Tile {
    Split { dir: SplitDir, ratio: f32, a: Box<Tile>, b: Box<Tile> },
    Leaf  { group: TabGroup },
}

struct TabGroup {
    tabs:       Vec<TabId>,
    active:     usize,   // the one that is drawn
    multi_exec: bool,    // broadcast mode for this tile
}
```

This lives in `polyterm-ui`. It is presentation state, not session state: `polyterm-core`
knows about sessions, not about where they sit on screen. `polyterm-store` persists a
serialised form of the tree (FR-95) alongside the session tree, referencing sessions by
`SessionId`. A restored layout naming a session that no longer exists drops that leaf and
loads the rest; it does not fail the restore.

Every tab in a group is **live**, not just the visible one. A background tab keeps its
transport, keeps draining output into its `Term`, and keeps its grid current. That is what
makes broadcasting to a tab you cannot see meaningful — and it is why FR-93 requires
background recipients to be marked as receiving.

All tabs in a group are sized to their tile, hidden ones included, so moving a divider
sends `ControlMsg::Resize` to every tab in the group. Terminals in different tiles have
different dimensions, and that is fine: each owns its own grid.

### 10.2 Where input goes

```
egui key / paste event
        │
        ▼
  focused tile ──── multi_exec = false ──► active tab only
        │
        └───────── multi_exec = true  ──► every terminal in the subtree:
                                          every tab of this group, then
                                          recursively every descendant leaf
```

The fan-out happens **here, in the UI layer, and nowhere else**. It is a loop over
`TransportHandle.input` senders. No backend and nothing in `polyterm-core` has any concept
of multi-exec; a transport cannot distinguish a broadcast keystroke from a typed one.
Keeping it that way is what stops broadcast from being reimplemented once per protocol.

Resolving the recipient set walks the tree **downward** from the focused node. It never
walks up. There is no parent pointer consulted during delivery and no global recipient
registry — the isolation FR-90 demands is a property of the traversal itself, not a filter
applied to a wider set afterwards. That distinction is the whole safety argument; a
filtered global list is one bad predicate away from typing into production.

### 10.3 Backpressure on a broadcast

`TransportHandle.input` is bounded like everything else. Broadcasting to eight sessions
where one is wedged must not stall the other seven and must not block the paint loop.

Delivery is `try_send` per recipient. On a full channel the input goes to a small
per-session pending queue drained on the next frame; if that queue is also full, the
session is marked in the UI as dropping input and the keystroke is discarded.

Discarding is correct here and the alternative is worse: an unbounded queue eventually
delivers a burst of stale keystrokes to a host minutes after they were typed. Say so in
the UI, do not buffer indefinitely, and never `send().await` on the UI thread.

### 10.4 Pastes

A paste resolves to a recipient set exactly as a keystroke does. FR-15's newline
confirmation fires once, before fan-out, and names how many sessions will receive it.
Bracketed-paste wrapping is then applied per session, since whether a session is in
bracketed-paste mode is a property of its own terminal state, not of the tile.

## 11. What is deliberately not here

- **No X server.** MobaXterm's X11 layer is a Windows-only workaround. On Linux it is
  redundant; on Windows, X11 forwarding is out of scope for v1. See `DECISIONS.md`.
- **No plugin system.** Not until the core is stable.
- **No VNC or SPICE.** The `RemoteDesktop` trait leaves room for them. v1 does not use it.
- **No session sharing or multi-user features.** This is a single-user desktop tool.
