# SPIKE-RDP.md

**Status: IN PROGRESS** — viewer built and verified on Windows/MSVC (2026-09-09).
Steps 2–5 need real hosts and a human; see *Hand-off* at the end.

This spike resolves ADR-2, which is `Provisional`. It is Milestone M0 and it blocks M8.
Budget one day.

---

## What is being decided

Whether `ironrdp` is good enough to be the RDP backend, or whether we take FFI bindings to
`libfreerdp` instead.

This is an empirical question. It cannot be answered from documentation and it must not be
answered by assumption in either direction.

### What the build found (read this first)

The concern this spike was written around is out of date. ADR-2 said `ironrdp` covered only
the legacy bitmap codecs and not the GFX pipeline. Against upstream `f639145`
(2026-09-09), the position is:

| Capability | State in the default build |
|---|---|
| Graphics Pipeline (MS-RDPEGFX): surfaces, cache, ClearCodec, Planar, progressive RemoteFX | **Implemented and on by default** (`ironrdp-egfx`, wired in `ironrdp-client`) |
| AVC420 (H.264 4:2:0) | Decodes **only** with an optional OpenH264 integration. Not in the default build. |
| AVC444 (H.264 4:4:4, what Windows prefers for text) | **Does not decode.** Upstream deliberately does not advertise it. |
| Legacy path: raw, interleaved RLE, RDP 6.0, RemoteFX | Present, used when GFX is not negotiated |
| Clipboard, device redirection, audio, RDP-UDP, gateway, `.rdp` files | Present, feature-gated |

The default viewer passes no H.264 decoder, so the client advertises the GFX **V8**
capability set and a Windows host answers with ClearCodec / Planar / progressive RemoteFX
over GFX. That is the pure-Rust, no-C-dependency configuration — the one PolyTerm would
ship under NFR-2 — and it is what this spike measures.

The OpenH264 integration comes in two flavours, and they are not equivalent:

- `openh264-bundled` compiles Cisco's C source at build time. Needs a C compiler and NASM,
  and a source-compiled binary carries **no** H.264 patent coverage. Not a path we would
  ship.
- `openh264-libloading` keeps the build pure Rust and loads Cisco's prebuilt `openh264.dll`
  / `libopenh264.so` at runtime if present. Cisco's binary carries patent coverage under its
  own licence, which requires the end user to fetch the binary. This is upstream's
  recommended distribution path and it is compatible with NFR-2 as written: a single
  executable, and an *optional* runtime asset rather than a daemon or a C build step.

That third option is why the decision rule below has three outcomes instead of two.

---

## Test targets

Fill this in before starting. Use hosts configured the way your real ones are, with your
actual GPO settings applied — not a freshly imaged VM with defaults, which is the most
common way this kind of spike produces a misleading pass.

| # | Host | OS / build | Auth | Security layer | GPO notes |
|---|---|---|---|---|---|
| 1 | | Windows 11 | | NLA | |
| 2 | | Windows Server (session host) | | NLA | |
| 3 | | Linux `xrdp` | | | |
| 4 | | *(optional)* Windows 11 over VPN / high latency | | NLA | |

Include at least one host reached over the network path you will actually use. A gigabit
LAN result tells you nothing about the VPN case, and codec efficiency is exactly where
bandwidth matters most.

---

## Procedure

### 1. Build the upstream viewer — DONE

Upstream: `Devolutions/IronRDP` at `f639145` (2026-09-09). The GUI binary is the
`ironrdp-viewer` crate (softbuffer + winit, no GPU). `ironrdp-client` is the library engine
it sits on; `ironrdp-client-glutin` is a separate, experimental GPU client.

```
cargo build --release -p ironrdp-viewer      # Windows, MSVC 14.51, Build Tools 2026
```

- 1 m 22 s, 24.8 MB, default features (`rustls`). **No C compilation** in the build.
- Installed to `~/.cargo/bin/ironrdp-viewer.exe` (on `PATH`). Source and target dirs are
  under `%LOCALAPPDATA%\Temp` and are disposable.
- Smoke test: a connection to a closed port exits 0 in 2 s with a clean
  `ConnectionRefused` in the log file. Connect path and `--log-file` work.
- Upstream also publishes prebuilt, checksummed viewer binaries on GitHub Releases
  under `ironrdp-viewer-v*` tags, if a second machine needs one.

The Linux build has **not** been done. Do it before writing the verdict — CredSSP differs
across platforms and the crate rules in `CLAUDE.md` 5 apply to spikes too.

### 2. Connect to each target

Do not type a password into a shell history or a chat. The viewer reads credentials from
the environment:

```powershell
$env:RDP_HOSTNAME = "host:3389"
$env:RDP_USERNAME = "user"           # or DOMAIN\user, or --domain
$env:RDP_PASSWORD = Read-Host -AsSecureString | ConvertFrom-SecureString -AsPlainText
$env:IRONRDP_LOG  = "info,ironrdp_connector=debug,ironrdp_egfx=debug"
ironrdp-viewer --log-file spike-host1.log
```

Useful flags (`ironrdp-viewer --help` for the rest): `--width` / `--height` for a fixed
resolution, `--codecs remotefx:on` to force or disable legacy bitmap codecs for an A/B,
`--no-credssp` only to *confirm* that NLA was what you were testing before.

For each host, record from the log — do not guess:

| Question | Where the answer is |
|---|---|
| Did it connect? | `ERROR ironrdp_viewer::app` on failure; a window on success |
| Which security layer? | `Server confirmed connection selected_protocol=…` (`ironrdp_connector`, info). `HYBRID` = NLA over TLS. |
| Did NLA happen? | `Begin NLA using CredSSP` (`ironrdp_connector`, debug) |
| Was the GFX pipeline negotiated, and at which version? | `EGFX capabilities confirmed` (`ironrdp_egfx`, debug). Expect **V8**. No such line means the legacy path. |
| Which codec on the legacy path? | Add `ironrdp_session=trace`; look for `Surface bits codec_id=…` |
| Did the host ask for something unsupported? | `Forwarding unsupported codec to handler` (`ironrdp_egfx`, trace) — and visible corruption |

### 3. Measure

| Metric | How | Bar |
|---|---|---|
| Connect time to desktop | Stopwatch, three attempts | Under 5 s on LAN |
| Text legibility | Open Notepad and a PowerShell window at 100% and 150% scaling | No visible ringing or smearing on small text |
| Scroll smoothness | Scroll a long document continuously | No tearing; no multi-second catch-up |
| Window drag latency | Drag a window across the desktop | Under ~150 ms perceived |
| Bandwidth at idle | OS network counters, 60 s idle session | Note it; compare to `mstsc` / `xfreerdp` |
| Bandwidth under scroll | Same, during continuous scroll | Note it; compare to `mstsc` / `xfreerdp` |

Run the same measurements with `mstsc` (Windows) or `xfreerdp` (Linux) against the same
hosts on the same network. The comparison is the point — absolute numbers without a
baseline do not decide anything.

### 4. The subjective test

Do thirty minutes of real work in an `ironrdp-viewer` session. Not a benchmark — actual
work. Edit a file, navigate a file manager, use a terminal, read something. This catches
what the metrics miss, and it is the test that should carry the most weight.

### 5. Feature checks

| Feature | Requirement | Works? | Notes |
|---|---|---|---|
| NLA / CredSSP negotiates | FR-60 | | |
| Certificate prompt on self-signed | FR-61 | | |
| Non-US keyboard layout, modifiers, extended keys | FR-62 | | |
| Mouse: all buttons + wheel | FR-63 | | |
| Clipboard, both directions, text | FR-64 | | |

FR-62 deserves particular attention. RDP is scancode-based, and a client that looks
correct under a US layout can be badly broken under others. Test with the layout you
actually use.

### 6. *(Optional)* What does H.264 buy?

Only if step 4 was borderline. `ironrdp-client-glutin` takes `--openh264-path` and loads
Cisco's prebuilt binary at runtime — the `libloading` path. Fetch the binary from Cisco's
release page (their EULA applies), build with `cargo build --release -p
ironrdp-client-glutin`, and repeat steps 3–4. The delta between this and the default viewer
is the value of AVC420. Remember that AVC444 is still absent either way: text in an AVC420
session is chroma-subsampled and may look slightly softer than `mstsc`.

---

## Decision rule

Write the verdict below. State it plainly; do not hedge it into uselessness.

**Go with `ironrdp`, pure Rust, if:** it connects to every target, NLA works on both
platforms, the five feature checks pass, and thirty minutes of real work in the default
build was not annoying. This is the outcome to hope for: no C anywhere, nothing optional to
ship.

**Go with `ironrdp` + `openh264-libloading` if:** the default build was borderline and
step 6 showed AVC420 fixes it. The build stays pure Rust; `polyterm-rdp` loads Cisco's
binary if the user has installed it and silently falls back to GFX-without-H.264 if not.
Record in ADR-2 that NFR-2 tolerates an optional runtime DLL, and add Cisco's notice to the
third-party licences. AVC444 remains unavailable and that must be stated in the docs.

**Go with `libfreerdp` FFI if:** any target fails to connect, NLA fails on Windows,
keyboard handling is wrong under your layout, or the subjective test was frustrating even
with H.264. This now requires a real failure, not just "worse than `mstsc`".

**Ambiguous result:** structure `polyterm-rdp` for both backends behind a Cargo feature,
ship `ironrdp` as the default, and keep FreeRDP as the escape hatch. The trait in
`polyterm-core` is what makes that one extra crate rather than a rewrite.

Bandwidth being somewhat worse than `mstsc` is not by itself a no-go. Bandwidth being bad
enough that the session is unpleasant over your actual network path is.

---

## Verdict

> *Fill in on completion. Then update ADR-2 in `DECISIONS.md` to `Accepted` or
> `Superseded by ADR-n`, and update `ARCHITECTURE.md` §3.2 if the trait needs to change
> to accommodate what was learned.*

**Date:**

**Decision:**

**Evidence:**

**Consequences for the roadmap:**

---

## Hand-off: what remains and who does it

Done by the build: step 1 on Windows; the codec landscape above; ADR-2's stale premise
corrected. Everything below needs your hosts, your credentials, and your eyes.

1. Fill in the target table. At least one host over the real network path.
2. Run step 2 against each, keeping the log files. Paste the `selected_protocol` and
   `EGFX capabilities confirmed` lines into the table.
3. Steps 3–5. Step 4 is the one that matters.
4. Build and run the viewer on Linux at least once before the verdict.
5. Write the verdict. Update ADR-2.

---

## If the answer is `libfreerdp`

What changes, so that this is scoped rather than feared:

- `polyterm-rdp` gains a `bindgen` build script against FreeRDP headers.
- Windows builds need FreeRDP available at link time — `vcpkg` is the usual route.
- Linux packaging gains a runtime dependency on `libfreerdp`, which every distribution
  already ships.
- NFR-2 is amended: still a single executable, but no longer free of C dependencies.
- ADR-1's reasoning is unaffected. Rust remains correct for every other reason.

What does **not** change: the `RemoteDesktop` trait, the UI, the tab infrastructure, the
crate graph, and every other milestone. That is the entire point of defining the seam
before writing the implementation.
