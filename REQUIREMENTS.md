# REQUIREMENTS.md

Requirements are numbered and stable. Reference them in commits and PRs. Do not renumber;
mark superseded items as such and add new IDs.

**Status key:** `MVP` = required for v1.0 · `v1.1` = deferred · `LATER` = acknowledged,
unscheduled · `OUT` = explicitly out of scope · `SUPERSEDED` = replaced by a later ID,
retained so the number is never reused.

---

## Functional — Application shell

| ID | Status | Requirement |
|---|---|---|
| FR-1 | MVP | The application presents a persistent session tree in a side panel, organised into user-created folders. |
| FR-2 | MVP | Sessions open as tabs. Multiple tabs may be open simultaneously, including multiple tabs on the same saved session. |
| FR-3 | SUPERSEDED | Tabs can be split horizontally and vertically into panes within one window. Superseded by FR-85 through FR-89, which replace flat pane splitting with a recursive tile tree. |
| FR-4 | MVP | Tab and tile layout survives application restart when the user opts in to session restore. What exactly is persisted is specified in FR-95. |
| FR-5 | MVP | The session tree supports create, rename, duplicate, move between folders, and delete. |
| FR-6 | MVP | Sessions are searchable by name and hostname from a single keyboard-driven filter. |
| FR-7 | v1.1 | Sessions can be exported to and imported from a portable file format. |
| FR-8 | v1.1 | Import from PuTTY saved sessions (registry on Windows, `~/.putty` on Linux) and from `~/.ssh/config`. |

## Functional — Terminal

| ID | Status | Requirement |
|---|---|---|
| FR-10 | MVP | The terminal emulates xterm-256color to the level of passing the bulk of `vttest` sections 1–3. |
| FR-11 | MVP | UTF-8 throughout, including wide (CJK) and combining characters. |
| FR-12 | MVP | Scrollback of a configurable size, default 10,000 lines, with mouse-wheel and keyboard navigation. |
| FR-13 | MVP | Mouse reporting modes (X10, SGR) so that `vim`, `htop`, and `tmux` behave correctly. |
| FR-14 | MVP | Copy on selection and paste via keyboard and mouse, integrated with the system clipboard. |
| FR-15 | MVP | Bracketed paste, with a confirmation prompt when pasted content contains a newline. |
| FR-16 | MVP | Configurable font family, size, and colour scheme, applied per-session or globally. |
| FR-17 | MVP | Terminal search within scrollback. |
| FR-18 | v1.1 | OSC 8 hyperlink support and clickable URL detection. |
| FR-19 | v1.1 | Sixel or Kitty graphics protocol. |

## Functional — SSH

| ID | Status | Requirement |
|---|---|---|
| FR-20 | MVP | Connect over SSH-2 with password, public key, and keyboard-interactive authentication. |
| FR-21 | MVP | Encrypted private keys are supported, with passphrase prompting and optional keyring storage. |
| FR-22 | MVP | SSH agent authentication: Unix domain socket on Linux, named pipe (OpenSSH agent) and Pageant on Windows. |
| FR-23 | MVP | Host key verification against a known-hosts store, with an explicit prompt on unknown or changed keys. A changed key is a blocking warning, not a passive notice. |
| FR-24 | MVP | Local port forwarding (`-L`). |
| FR-25 | MVP | Remote port forwarding (`-R`). |
| FR-26 | MVP | Dynamic SOCKS proxy forwarding (`-D`). |
| FR-27 | MVP | Forwards are owned by the session, are listed with live status, and can be added or removed while connected. |
| FR-28 | MVP | Jump host / `ProxyJump` chaining, at least one hop deep. |
| FR-29 | MVP | Keepalive and automatic reconnect with configurable backoff. |
| FR-30 | v1.1 | X11 forwarding on Linux. |
| FR-31 | LATER | Mosh support. |

## Functional — SFTP

| ID | Status | Requirement |
|---|---|---|
| FR-35 | MVP | An SFTP browser pane attached to an SSH session, sharing the same authenticated connection. |
| FR-36 | MVP | Directory navigation, and upload/download of files and directories with progress reporting. |
| FR-37 | MVP | Create, rename, delete, and chmod on the remote side. |
| FR-38 | MVP | Drag and drop between the local filesystem and the SFTP pane. |
| FR-39 | v1.1 | Edit-in-place: open a remote file in a local editor and write back on save. |
| FR-40 | v1.1 | Transfer queue with pause, resume, and retry. |

## Functional — Serial / RS-232

| ID | Status | Requirement |
|---|---|---|
| FR-45 | MVP | Enumerate available serial ports on both platforms, with device description where the OS provides one. |
| FR-46 | MVP | Configure baud rate, data bits, parity, stop bits, and flow control (none, hardware, software) per session. |
| FR-47 | MVP | Manual control of DTR and RTS, and the ability to send a BREAK. |
| FR-48 | MVP | Display of CTS, DSR, DCD, and RI line states. |
| FR-49 | MVP | Detect device disconnection (USB serial unplug) and surface it without terminating the tab, allowing reconnect when the port returns. |
| FR-50 | MVP | Session logging to file, with timestamps, toggleable at runtime. |
| FR-51 | v1.1 | Hex view mode alongside the terminal view. |
| FR-52 | v1.1 | Send-file over serial with XMODEM/YMODEM. |

## Functional — Local shell

| ID | Status | Requirement |
|---|---|---|
| FR-55 | MVP | Local shell tabs using the platform PTY (ConPTY on Windows, forkpty on Linux). |
| FR-56 | MVP | Configurable default shell and startup working directory. |

## Functional — RDP

| ID | Status | Requirement |
|---|---|---|
| FR-60 | MVP | Connect to Windows RDP hosts with NLA / CredSSP enabled. |
| FR-61 | MVP | Server certificate verification with an explicit user prompt on mismatch or self-signed certificates. |
| FR-62 | MVP | Full keyboard passthrough using scancodes, correct under non-US layouts, including modifier and extended keys. |
| FR-63 | MVP | Mouse input including wheel and all three buttons. |
| FR-64 | MVP | Bidirectional text clipboard. |
| FR-65 | MVP | Fixed resolution selected at connect time, with scaling-to-fit in the pane. |
| FR-66 | v1.1 | Dynamic resolution change on pane resize. |
| FR-67 | v1.1 | Clipboard file transfer and drive redirection. |
| FR-68 | v1.1 | Audio redirection. |
| FR-69 | LATER | Multi-monitor. |
| FR-70 | OUT | RemoteApp / seamless window mode. |

## Functional — Cross-cutting

| ID | Status | Requirement |
|---|---|---|
| FR-75 | SUPERSEDED | Multi-exec: type once, broadcast to a selected set of open terminal tabs. Superseded by FR-90 and FR-91, which bind the broadcast set to a tile instead of to a free-floating selection. Still a primary reason the tool exists; still not optional. |
| FR-76 | MVP | Credentials stored in the OS keyring, referenced by the session store, never written to disk by us. |
| FR-77 | MVP | Per-session and global settings, editable in a UI, persisted immediately. |
| FR-78 | MVP | Keyboard-driven operation: new tab, close tab, next/previous tab, session search, tile splitting, and directional tile focus all bindable. |
| FR-79 | v1.1 | User-defined keyboard shortcut remapping. |
| FR-80 | v1.1 | Per-session startup command or macro sent on connect. |
| FR-81 | LATER | Scripting or plugin interface. |

---

## Functional — Tiling, focus, and input routing

The content area is a tiling layout, not a single tab strip. This block replaces FR-3 and
rescopes FR-75.

| ID | Status | Requirement |
|---|---|---|
| FR-85 | MVP | The content area is a recursive tile tree. Any tile can be split horizontally or vertically, and the result split again, so that many sessions are visible and live at the same time. |
| FR-86 | MVP | A tile holds either a single session view or a tab group with its own tab bar and one visible tab. Terminal, SFTP, and RDP views are all tileable. |
| FR-87 | MVP | Tiles can be split, closed, and resized by dragging the divider between them. Closing a tile promotes its sibling into the vacated space; the tree never leaves a hole. |
| FR-88 | MVP | Tabs can be dragged between tab groups, and dropped onto a tile edge to split that tile and land in the new one. |
| FR-89 | MVP | Exactly one tile holds input focus at any moment. Focus moves by click and by directional keyboard binding (FR-78). The focused tile is visually unambiguous. |
| FR-90 | MVP | **Input isolation.** Keystrokes, pastes, and clipboard actions are delivered only to sessions inside the focused tile. Input never crosses a tile boundary, and no global broadcast mode exists that could make it do so. |
| FR-91 | MVP | **Tile-scoped multi-exec.** Multi-exec is a per-tile mode. With it enabled on a tile, input directed at that tile goes to every terminal session that tile contains — every tab of its tab group, and every terminal in its descendant tiles if it has been split. |
| FR-92 | MVP | Multi-exec delivers to terminal sessions only: SSH, serial, and local shell. RDP and SFTP views inside a multi-exec tile receive nothing. |
| FR-93 | MVP | A tile with multi-exec active is unmistakably marked, and so is every session receiving broadcast input, including tabs that are not currently visible. Enabling multi-exec is always a deliberate act, never a side effect of a layout change. |
| FR-94 | MVP | A paste that requires confirmation under FR-15 is confirmed once for the whole broadcast, stating how many sessions will receive it — not once per session. |
| FR-95 | MVP | The tile tree is what FR-4 persists: structure, split ratios, each tile's tab set and active tab, and which tile held focus. Multi-exec state is never restored as enabled. |
| FR-96 | v1.1 | Per-session opt-out from broadcast while the session remains in a multi-exec tile. |
| FR-97 | v1.1 | Named tile layouts that can be saved and reapplied to a set of sessions. |
| FR-98 | v1.1 | Tear a tile out into a separate OS window. Isolation between windows is at least as strict as between tiles. |

---

## Non-functional

| ID | Status | Requirement |
|---|---|---|
| NFR-1 | MVP | Runs on Linux (x86-64, glibc) and Windows 10/11 (x86-64, MSVC toolchain). |
| NFR-2 | MVP | Ships as a single executable per platform plus its assets. No runtime interpreter, no bundled daemon, no C service process. |
| NFR-3 | MVP | Cold start to interactive window under 1 second on modest hardware. |
| NFR-4 | MVP | The UI thread never blocks. No user-visible stall exceeds 100 ms, including during connection establishment, DNS resolution, or host key lookup. |
| NFR-5 | MVP | Sustained terminal throughput: `cat` of a 10 MB text file over SSH renders without dropping below 30 fps or exhausting memory. |
| NFR-6 | MVP | Memory use with 20 idle open sessions stays under 500 MB. |
| NFR-7 | MVP | A hung or dead connection never freezes the application or other sessions. |
| NFR-8 | MVP | No secret appears in log output at any level, in `Debug` formatting, or in crash reports. |
| NFR-9 | MVP | `cargo clippy --workspace --all-targets -- -D warnings` is clean. |
| NFR-10 | MVP | `polyterm-term` is testable headless: byte sequence in, grid state assertions out. |
| NFR-11 | MVP | Free and open source. No component with a licence incompatible with that. |
| NFR-12 | v1.1 | High-DPI and fractional scaling correct on both platforms, including mixed-DPI multi-monitor. |
| NFR-13 | v1.1 | Accessible keyboard navigation of all UI chrome without a mouse. |
| NFR-14 | MVP | Eight terminal tiles, all visible and all receiving output, render without the UI dropping below 30 fps. Damage tracking is per tile; an idle tile costs nothing to keep on screen. |
| NFR-15 | MVP | Input isolation (FR-90) is enforced structurally and covered by tests. A keystroke reaching a session outside the focused tile is a correctness bug of the highest severity, not a cosmetic one. |
| NFR-16 | MVP | A session whose input channel is full must not stall broadcast delivery to its siblings and must never block the UI thread. Slow consumers are handled per session. |

---

## Explicitly out of scope

| ID | Requirement |
|---|---|
| OUT-1 | Bundled X server. MobaXterm's X11 layer is a Windows-only workaround; on Linux it is redundant. |
| OUT-2 | Bundled Unix userland (Cygwin equivalent). |
| OUT-3 | Telnet, rlogin, and other unencrypted remote shell protocols. |
| OUT-4 | Any cloud sync, telemetry, or account system. |
| OUT-5 | Mobile or web clients. |
| OUT-6 | Acting as a server for any of these protocols. |

---

## Acceptance targets for v1.0

The release bar, stated as things a user does rather than features that exist:

1. Save an SSH session, connect with a keyring-stored key passphrase, run `htop`, resize
   the window, and have the display stay correct.
2. Open a serial console to a board at 115200 8N1, toggle DTR to reset it, watch the boot
   log, unplug the USB adapter, plug it back in, and reconnect without restarting.
3. Split a tile four ways, open an SSH session in each, enable multi-exec on the parent
   tile, run one command, and see four results side by side — while a fifth session in an
   adjacent tile receives nothing.
4. Browse a remote filesystem over SFTP in the same connection as an open shell, and
   drag a file down to the desktop.
5. Connect to a Windows 11 host over RDP with NLA, log in, copy text out to the local
   clipboard.
6. Do all of the above on both Linux and Windows from the same session store.
