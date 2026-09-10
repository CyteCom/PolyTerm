# DECISIONS.md

Architecture decision records. Each entry states what was decided, why, what was
rejected, and what would cause us to revisit. The purpose of this file is to stop
settled questions from being reopened in every session.

Format: `ADR-n · Status · Title`. Statuses: `Accepted`, `Superseded by ADR-n`,
`Provisional` (decided, but with an open verification gate).

---

## ADR-1 · Accepted · Rust as the implementation language

**Decision.** Rust, stable channel, edition 2024.

**Why.** The choice was driven almost entirely by two constraints: the availability of an
embeddable terminal emulator, and how RDP pixels get on screen. Rust is the only
ecosystem that answers both without a C dependency. `alacritty_terminal` and
`wezterm-term` are the extracted cores of two production terminals. `ironrdp` is the only
modern RDP protocol implementation outside FreeRDP itself. SSH (`russh`), serial
(`serialport`), and PTY (`portable-pty`) are all mature and all work on Windows.

**Rejected.**

- **Java.** No maintained RDP client exists; everything descends from `properJavaRDP`,
  which implements RDP 4/5 with no TLS or NLA and will not connect to current Windows
  hosts. RDP would have meant embedding an external `xfreerdp` window, which is
  X11-specific and dying with Wayland.
- **C++/Qt.** Technically the strongest option and the only toolkit with first-class
  foreign-window embedding, plus in-process `libfreerdp` linking. Lost on the Windows
  requirement: QTermWidget is Unix-only, so the terminal would have had to be rebuilt
  anyway, which was most of what Qt was buying.
- **Go.** Excellent SSH and serial libraries and a familiar language, but no mature
  terminal widget for any Go GUI toolkit and no serious Go RDP implementation. Would have
  meant writing a VT emulator from scratch and reaching RDP through cgo, surrendering
  Go's advantages.
- **Tauri / Electron.** `xterm.js` is the best terminal emulator available, but WebKitGTK
  is the weak link on Linux, native window embedding is impossible, and Electron is
  already Tabby.
- **C#/Avalonia.** Mature UI, but no terminal widget and no cross-platform RDP.

**Revisit if.** Not foreseen. This is foundational.

---

## ADR-2 · Accepted · `ironrdp` as the RDP implementation

**Decision.** Use `ironrdp-client` behind the `RemoteDesktop` trait in `polyterm-core`.

**Accepted 2026-09-09** on the strength of the M0 spike (`SPIKE-RDP.md`). The open gate —
whether the codec coverage is good enough — was closed empirically: a real authenticated
session against a Windows Server 2019 host over VPN was pleasant, with crisp small text and
snappy input, *even though it ran over the legacy bitmap path with no GFX or H.264*. The
pure-Rust build ships with no H.264 decoder; `openh264-libloading` is held in reserve at no
cost to NFR-2. NLA verified at runtime on Windows against two hosts. No FreeRDP FFI.

**Why.** It is a pure-Rust RDP implementation from Devolutions with security as its
stated focus. `ironrdp-client` is factored as a library-only, event-loop-agnostic engine
that emits output on a channel for an embedder to consume — precisely the shape needed
for a tabbed session manager. It keeps the single-binary, no-C-dependency property.

**The open gate.** *Corrected during M0 against upstream `f639145`, 2026-09-09.* The
Graphics Pipeline (MS-RDPEGFX) is implemented — surfaces, caching, ClearCodec, Planar,
progressive RemoteFX — and is on by default. H.264 is partial: AVC420 decodes only through
an optional OpenH264 integration (compiled from C, or Cisco's prebuilt DLL loaded at
runtime), and AVC444 does not decode at all; upstream deliberately does not advertise it.
The pure-Rust build therefore negotiates GFX without H.264. That is a far better path than
the legacy bitmap codecs this ADR originally described, and it widens the decision from
two options to three (see `SPIKE-RDP.md`). Whether it is good enough is still an empirical
question against real target hosts, not a judgement call.

**Rejected.**

- **Spawning `xfreerdp` and reparenting its window.** Platform-specific, X11-only,
  incompatible with Wayland, and unavailable in every Rust GUI toolkit.
- **`guacd` plus the Guacamole protocol.** Bounded and well-documented work, but adds a C
  daemon as a runtime dependency, which defeats NFR-2.
- **Writing RDP from scratch.** FreeRDP is roughly half a million lines of C. No.

**Revisit if.** A real target proves unworkable in a way the spike did not surface — a host
that only offers the full H.264/AVC444 GFX pipeline and is unusable without it, or the
`ReactivationTimedOut` dynamic-resize defect turning out to be engine-deep rather than
viewer front-end behaviour (M8 will tell). The fallback remains FFI bindings to `libfreerdp`
via `bindgen`, behind the same trait — losing the pure-Rust property and gaining a C build
dependency on both platforms, but changing nothing else in the project. This is exactly why
the trait exists and why it was defined before any RDP code was written.

---

## ADR-3 · Accepted · `egui` / `eframe` on `wgpu` for the UI

**Decision.** Immediate-mode UI via `eframe`, rendering through `wgpu`.

**Why.** The two heavy surfaces in this application — the terminal grid and the RDP
framebuffer — are both textures we blit ourselves. GPU-backed rendering matters more than
widget richness. `egui` is mature, cross-platform, and pure Rust.

**Known cost.** Immediate-mode is weakest at exactly what a connection manager needs most:
dense desktop chrome, tree views with context menus, dockable and splittable panes. Expect
to fight the UI layer rather than the protocols. Budget for it.

**Rejected.** Slint (triple-licensing complicates a free-software release), GTK4 (removed
`GtkSocket`/`GtkPlug`, weaker Windows story), Iced (less mature ecosystem at the time of
decision), native-per-platform (doubles all UI work).

**Revisit if.** The pane and tree work in Milestone 4 proves genuinely intractable. Moving
to Slint would touch only `polyterm-ui` if the crate boundaries in `ARCHITECTURE.md` have
been respected.

---

## ADR-4 · Accepted · `alacritty_terminal` for VT emulation

**Decision.** `alacritty_terminal` as the terminal state machine, wrapped by
`polyterm-term`. We render; it parses and holds the grid.

**Why.** Production-proven, actively maintained, ships on Windows, exposes damage
tracking, and is a pure state machine with no I/O — which makes it directly unit-testable
against NFR-10.

**Rejected.** `wezterm-term` (comparable and a reasonable substitute; `alacritty_terminal`
has the leaner API surface), `vte` alone (parser only, no grid model), writing our own
(a VT220-through-xterm emulator is a multi-month project and a solved problem).

**Revisit if.** Damage-tracking granularity proves insufficient for NFR-5. `wezterm-term`
is the drop-in alternative and swapping it touches one crate.

---

## ADR-5 · Accepted · One uniform `Transport` trait for SSH, serial, and PTY

**Decision.** SSH shells, serial ports, and local PTYs all present the same channel-based
handle: bounded output, input, control, events.

**Why.** They are the same thing — a bidirectional byte stream with out-of-band control
and lifecycle events. Unifying them means the terminal pane, session logging, multi-exec
broadcast, and reconnect logic are each written once rather than three times. The
differences (serial has no resize; SSH has host keys; PTY has neither) are expressed as
control messages a backend may ignore and events it may never emit.

**Rejected.** Per-protocol pane types, which is how most connection managers end up with
divergent behaviour between their SSH and serial tabs.

**Consequence to accept.** `ControlMsg::Resize` being a no-op on serial is correct. Do not
"fix" it.

---

## ADR-6 · Accepted · Channels only between the UI thread and the runtime

**Decision.** No shared locks between `eframe`'s main thread and the tokio runtime. All
cross-boundary communication is bounded channels. Wake the UI with `request_repaint()`.

**Why.** This is the property that satisfies NFR-4 and NFR-7. A `Mutex` shared with a task
that is blocked on a dead TCP connection eventually freezes the paint loop; a channel
cannot. Bounding the channels also gives backpressure for free, which is what keeps NFR-5
from becoming unbounded memory growth.

**Rejected.** `Arc<Mutex<TerminalState>>` read by the renderer — simpler to write, and the
direct cause of the freeze-on-dead-connection bug in most naive implementations.

---

## ADR-7 · Accepted · `x86_64-pc-windows-msvc` only; no cross-compilation for development

**Decision.** The MSVC toolchain is the only supported Windows target. Development and
testing happen on a real Windows machine, not via cross-compilation from Linux.

**Why.** `ironrdp`'s CredSSP path uses Windows SSPI, and the `libfreerdp` fallback in
ADR-2 links cleanly only against MSVC. Beyond linking: `cargo-xwin` will produce a Windows
binary from Linux, but a GUI application cannot be debugged that way and RDP negotiation
against a real host cannot be tested that way.

**Rejected.** The `windows-gnu` toolchain (persistent linking friction, worse debugger
support), cross-compilation as the primary workflow (untestable).

---

## ADR-8 · Accepted · OS keyring for credentials; no homegrown vault

**Decision.** Secrets go to the platform credential store via the `keyring` crate —
Windows Credential Manager, Secret Service or KWallet on Linux. The session store holds
only a reference.

**Why.** Writing a credential vault means designing key derivation, at-rest encryption,
and an unlock flow, and getting any of it subtly wrong is worse than not having one. The
platforms already solved this.

**Rejected.** An encrypted local file with a master password. Explicitly rejected as a
fallback too: if the keyring is unavailable, the correct behaviour is to prompt every time,
not to silently write secrets somewhere weaker.

**Consequence.** Secrets still pass through our memory — a password on its way from the
keyring to `russh`. In transit they are held in `Secret<T>`, which cannot be `Debug`- or
`Display`-printed, is neither `Clone` nor `Serialize`, and is zeroised on drop through the
`zeroize` crate (pure Rust, no dependencies of its own). There is no accessor that moves
the value out un-zeroised. How a secret reaches a backend at all is `ARCHITECTURE.md` §6:
backends have no keyring access and never will.

---

## ADR-9 · Accepted · SQLite for the session store

**Decision.** `rusqlite`, in the platform config directory via `directories`.

**Why.** The session tree needs partial updates, will grow, and must survive an unclean
shutdown. A JSON file rewritten wholesale on every change fails the third of those.

**Rejected.** JSON or TOML file (no atomicity, whole-file rewrites), a key-value store
(the tree is relational), a directory of per-session files (poor rename and move
semantics).

**Note.** Export to a portable text format is FR-7 and is a separate concern from the
storage format.

---

## ADR-10 · Accepted · No bundled X server, no bundled Unix userland

**Decision.** Out of scope permanently. See OUT-1 and OUT-2.

**Why.** MobaXterm's Cygwin and X11 layer is the answer to a Windows-specific problem —
the absence of a Unix environment. On Linux it is entirely redundant. On Windows, WSL now
covers it far better than we could. Reimplementing an X server is not a serious proposal.

**Consequence.** This means we are not a MobaXterm clone; we are a connection manager.
X11 forwarding on Linux (FR-30) is a separate and much smaller question.

---

## ADR-11 · Accepted · Crate boundaries follow substitution seams

**Decision.** The workspace is split so that each externally-sourced capability —
terminal engine, SSH stack, RDP stack, UI toolkit — lives behind a trait in
`polyterm-core` and is implemented in exactly one crate.

**Why.** Two reasons. First, ADR-2 is provisional and ADR-3 and ADR-4 have live revisit
conditions; the boundaries are what make those reversible for the cost of one crate rather
than a rewrite. Second, the crate boundaries double as context boundaries for agentic
development — a session can be scoped to one crate instead of the whole tree.

**Consequence.** `polyterm-ui` must not name `russh`, `serialport`, `portable-pty`, or
`ironrdp` in its manifest. This is checkable and should be checked in review.

---

## ADR-12 · Accepted · A recursive tile tree for the content area

**Decision.** The window's content area is a binary split tree. Any tile splits
horizontally or vertically and the result splits again. A leaf holds either one session
view or a tab group with its own tab bar.

**Why.** Two demands that look different are the same structure: "show me four terminals
at once" and "keep a stack of tabs in that corner". A tree covers both, and split ratios,
divider dragging, sibling promotion on close, and directional focus all fall out of it
without special cases. It is also the only layout model that persists meaningfully — FR-95
serialises a tree, not a screenshot.

**Rejected.**

- **A flat grid of panes.** Cannot express nesting, so "split this pane, but only this
  one" has no representation, and resize semantics get arbitrary fast.
- **Floating MDI child windows.** A known usability dead end, and layout restore becomes a
  pile of coordinates that is wrong on the next monitor.
- **A general docking framework.** `egui` has no mature one. ADR-3 already warns that this
  layer is where the fighting happens; adopting a heavyweight docking crate makes the ADR-3
  revisit condition harder to exercise, not easier.

**Realized with `egui_tiles` 0.17.1** (evaluated and adopted at M4). The deciding
question — whether its focus and drag-drop model can be constrained to satisfy FR-90 —
resolved decisively in its favour: *`egui_tiles` has no focus or input-routing model at
all.* It lays out and draws tiles and drives pointer-driven drag-drop; it never reads
keyboard input except one `Key::Escape` peek used solely to cancel an in-progress drag
(never during typing, and not consumed). A terminal receives a keystroke only because our
own `Behavior::pane_ui` reads `ctx.input()` — so FR-90 isolation is not a constraint we
impose on the crate but a gate we write in our own pane hook, keyed on a focused tile we
track ourselves. There is no competing input path that could leak across a tile boundary.

What the crate supplies that hand-rolling would have cost: linear (h/v) and grid split
layout, resizable dividers with hit-testing, the full drag-to-rearrange interaction (drop
zones, previews, insertion), scrolling tab bars, tree simplification, and `serde` on
`Tree<Pane>` (FR-95). What remains ours in both worlds and sits cleanly on top of
`pane_ui`: the focused-tile model + FR-90 gate, multi-exec broadcast (FR-96–98), and the
`Pane`→terminal glue. Its `Tabs` container exposes `children: Vec<TileId>` and
`active: Option<TileId>` as public fields, which is exactly what the recipient-set walk
(§10.2) and the "every tab is live" rule (§10.1) need. Version 0.17.1 tracks egui `^0.36`,
matching our pin; it is pure Rust and adds no C toolchain requirement.

**Rejected (additionally).**

- **Hand-rolling the tree** per `ARCHITECTURE.md` §10.1's `Tile` enum. Full control of the
  layout traversal, but it buys control over code the safety property never touches —
  FR-90 lives in our `pane_ui`, not in the crate's layout walk — at the price of
  reimplementing the drag-drop, resize, tab-bar, and persistence machinery `egui_tiles`
  already ships. The §10.1 enum stays as the conceptual model; `egui_tiles` realizes it.

**Consequence to accept.** One load-bearing dependency, pinned to `egui_tiles`'s release
cadence for future egui bumps (it has tracked egui 0.32→0.36 promptly). Our persisted
layout format becomes partly `egui_tiles`'s, mitigated by using our own `SessionId` as the
`Pane` payload so a restore can rebuild against the session store (§10.1, FR-95).

**Revisit if.** `egui_tiles` falls behind an egui release we must take, or M4 shows the
tree cannot be driven from `egui`'s immediate-mode input handling without per-frame layout
thrash. ADR-3's revisit condition still applies: this layer is where the fighting happens.

---

## ADR-13 · Accepted · The tile is the broadcast domain

**Decision.** Multi-exec is a per-tile mode. Input delivered to a tile with multi-exec on
reaches every terminal in that tile's subtree — every tab of its group and every terminal
in its descendant tiles. Input never crosses a tile boundary. There is no global broadcast
mode and no free-floating selection of target tabs. This supersedes FR-75.

**Why.** MobaXterm's multi-exec keeps its target set as per-tab state you cannot see at
the moment you type. That state is easy to leave enabled, and the failure mode is a command
executing on a host you were not looking at — the most expensive bug this class of tool
has. Binding the target set to the layout makes it spatial: the sessions receiving your
keystrokes are exactly the ones inside the highlighted rectangle. Isolation stops being a
flag that is checked and becomes a consequence of how delivery is resolved
(`ARCHITECTURE.md` §10.2).

**Rejected.**

- **Global multi-exec with per-tab enable flags** — the MobaXterm model. Invisible state,
  wrong-host risk, and no way to see the target set at a glance.
- **A "broadcast group" concept orthogonal to layout.** Two containment hierarchies to hold
  in your head, and the UI still cannot show you the target set without drawing it, at
  which point it is the layout again.

**Consequence to accept.** To broadcast to a set of sessions you must first arrange them
into one tile. That is one extra layout action, and it is the feature, not the tax — the
arrangement is the confirmation. It also means the recipient set can include background
tabs, which is why FR-93 requires them to be marked as receiving.

**Revisit if.** Users routinely need to broadcast to sessions they do not want adjacent on
screen. Try FR-97 (named layouts) before reintroducing a selection model.

---

## ADR-14 · Accepted · `serialport` without its `libudev` feature

**Decision.** Depend on `serialport` with `default-features = false`, which drops its
optional `libudev` C dependency. Linux port enumeration uses the crate's pure-Rust sysfs
fallback instead.

**Why.** `libudev` is a C library, and pulling it in would put a C build dependency on the
Linux target — the exact property the whole stack was chosen to avoid (ADR-1, NFR-2). The
feature is optional, and the sysfs fallback still yields port names and enough device
detail to satisfy FR-45 ("device description where the OS provides one"). Keeping the
build pure Rust is worth more than the marginal extra USB metadata `libudev` would supply.

**Rejected.** Enabling `libudev` for richer Linux descriptions. Reconsider only if the
sysfs data proves too thin to tell two adapters apart in practice.

**Consequence.** CI needs no `libudev-dev`. Windows and macOS are unaffected — their
backends are target-gated, not behind this feature.

---

## ADR-15 · Accepted · The `SessionSpawner` boundary opens sessions

**Decision.** The UI opens a session by handing a `SessionSpec` to a
`SessionSpawner` — a trait defined in `polyterm-ui` and implemented by
`apps/polyterm`. The binary matches the spec's kind to a backend, calls
`Transport::spawn`, and returns the protocol-erased `TransportHandle`. The UI
holds the spawner as `Arc<dyn SessionSpawner>` and never names a backend crate.

**Why.** ADR-11 forbids the UI from naming `russh`, `serialport`,
`portable-pty`, or `ironrdp`, yet the UI is where a session is opened (a click
in the session tree, FR-1/FR-2). Something must cross that gap. The `Transport`
trait itself is not object-safe (associated `Config`, associated `KIND`), so the
UI cannot hold a `dyn Transport`. A one-method, object-safe spawner is the
narrowest possible seam: the UI expresses *what* to open (a `SessionSpec`, a
`polyterm-core` type that holds no secret) and the binary decides *how*. It also
keeps the erasure exactly where ADR-11/§4 already put it — at the handle.

**Where it lives.** In `polyterm-ui`, not `polyterm-core`. Both the UI (caller)
and the binary (implementer) can see it there, and it is a UI-consumption
contract, not part of the transport vocabulary. This keeps `polyterm-core`
unchanged — no new core trait — while respecting the dependency layering.

**Rejected.**

- **A `dyn Transport` in the UI.** Not object-safe, and it would drag the
  backend's `Config` type into the UI regardless.
- **A `SessionSpawner` in `polyterm-core`.** Core would then reference
  `TransportHandle` construction on behalf of the UI for no gain; the contract
  is consumed at the UI boundary, so it belongs there.
- **The UI depending on the backend crates directly behind `#[cfg]`.** A flat
  violation of ADR-11, and it defeats the point of the erased handle.

**Consequence.** A failed *initial* open is no longer a fatal binary error; it
surfaces in the UI (the session panel shows the error) exactly as a later open
would, which is both more consistent and better UX than exiting. RDP has no
place here — it is a `RemoteDesktop`/`RdpHandle`, not a `Transport`, so the
spawner reports it unsupported until M8 gives RDP its own pane type.

**Revisit if.** Opening needs to be asynchronous at the call site (today it
returns a handle immediately and progress arrives as events). If a backend ever
needs to do blocking work before it can hand back a handle, the spawner returns
a future instead — a signature change, not a design change.
