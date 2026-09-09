# SPIKE-RDP.md

**Status: IN PROGRESS** — viewer built on Windows/MSVC and (underway) on Linux/WSL.
Two Windows hosts probed credential-free: security layer, NLA enforcement, and GFX
advertisement confirmed. The authenticated session and the subjective test need a human
with the password; see *Hand-off* at the end.

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
| 1 | 10.0.10.10 (`DESKTOP-EV3LKMN`) | Windows 11 24H2, build 26100 | jeffw, password | **`HYBRID_EX`** — confirmed by probe | NLA enforced: a bad credential fails inside CredSSP (`0xC000006D`) before any graphical logon. Server advertises `DYNVC_GFX_PROTOCOL_SUPPORTED` and restricted-admin mode. LAN, sub-millisecond RTT. |
| 2 | 10.166.250.209 (`WIN-2QQ6BE2KRG9`) | Windows Server 2019, build 17763 | Administrator, password | **`HYBRID_EX`** — confirmed by probe | NLA enforced (`0xC000006D` on a bad credential). Reached over **OpenVPN** (client 10.19.0.2), so this is also the real-WAN-path target row 4 asks for. Advertises `DYNVC_GFX_PROTOCOL_SUPPORTED` and `REDIRECTED_AUTHENTICATION_MODE_SUPPORTED` (Remote Credential Guard capable). Full CredSSP round-trip in 0.2 s over the VPN. |
| 3 | 10.0.10.12 | Ubuntu, OpenSSH 10.2p1 | — | — | **No `xrdp`**: 3389 closed. Becomes a target only if xrdp is installed. |
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

The Linux build was attempted in WSL (Ubuntu 26.04). It needs the X11/xkb dev headers
winit and softbuffer link against, and `sudo` in WSL is not passwordless here, so it is
blocked on one privileged step — run once:

```bash
wsl -d Ubuntu-26.04 sudo apt-get install -y build-essential pkg-config \
    libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev \
    libxcursor-dev libxrandr-dev libxi-dev
```

After that the toolchain install and `cargo build -p ironrdp-viewer` need no privileges.
Do it before writing the verdict — CredSSP differs across platforms and the crate rules
in `CLAUDE.md` 5 apply to spikes too.

### 2. Connect to each target

Do not type a password into a shell history or a chat. The viewer reads credentials from
the environment:

```powershell
$env:RDP_HOSTNAME = "host:3389"
$env:RDP_USERNAME = "user"           # or DOMAIN\user, or --domain
$env:RDP_PASSWORD = Read-Host -AsSecureString | ConvertFrom-SecureString -AsPlainText
$env:IRONRDP_LOG  = "info,ironrdp_egfx=debug"
ironrdp-viewer --log-file spike-host1.log
```

Useful flags (`ironrdp-viewer --help` for the rest): `--width` / `--height` for a fixed
resolution, `--codecs remotefx:on` to force or disable legacy bitmap codecs for an A/B,
`--no-credssp` only to *confirm* that NLA was what you were testing before.

For each host, record from the log — do not guess:

| Question | Where the answer is |
|---|---|
| Did it connect? | `ERROR ironrdp_viewer::app` on failure; a window on success |
| Which security layer? | `Server confirmed connection selected_protocol=…` (`ironrdp_connector`, info). `HYBRID` = CredSSP, i.e. NLA over TLS. |
| Did NLA succeed? | `selected_protocol=HYBRID` followed by a desktop. Only if it *fails*, add `ironrdp_connector=debug` for `Begin NLA using CredSSP` and the exchange — see the caution below. |
| Was the GFX pipeline negotiated, and at which version? | `EGFX capabilities confirmed` (`ironrdp_egfx`, debug). Expect **V8**. No such line means the legacy path. |
| Which codec on the legacy path? | Add `ironrdp_session=trace`; look for `Surface bits codec_id=…` |
| Did the host ask for something unsupported? | `Forwarding unsupported codec to handler` (`ironrdp_egfx`, trace) — and visible corruption |

**Caution on `ironrdp_connector=debug`.** It dumps the CredSSP exchange, and an NTLM
authenticate message carries an NTLMv2 response that is crackable offline. The plaintext
password is never logged — checked at `f639145`: upstream's `Credentials` derives `Debug`
without redaction, but nothing logs it and the client config strips secrets before exposing
itself — so a connector-debug log is sensitive rather than fatal. Do not share one. The
default filter above keeps the connector at info.

Note for M8: `polyterm-rdp` must never `Debug`-print the `ironrdp` config it builds. That
is exactly the NFR-8 hole `Secret<T>` exists to close on our side of the boundary.

#### Results so far — 10.0.10.10, credential-free probe (2026-09-09)

Run with a nonexistent username so nothing could touch a real account's lockout counter.
Everything below was negotiated before the credential was evaluated, which is why no
credential was needed to learn it.

- Client offered `SSL | HYBRID | HYBRID_EX`; server selected **`HYBRID_EX`** — CredSSP
  with early user authorisation, the modern NLA variant Windows 11 picks when NLA is
  required.
- Server response flags: `DYNVC_GFX_PROTOCOL_SUPPORTED` (the GFX pipeline will be
  negotiated once authenticated), `RESTRICTED_ADMIN_MODE_SUPPORTED`,
  `EXTENDED_CLIENT_DATA_SUPPORTED`.
- TLS handshake completed and CredSSP ran over it — NTLM NEGOTIATE / CHALLENGE /
  AUTHENTICATE — then the server returned `STATUS_LOGON_FAILURE (0xC000006D)` *inside*
  the CredSSP exchange. NLA is enforced; a non-NLA host would have fallen through to the
  graphical logon.
- The NTLM CHALLENGE identifies the host as `DESKTOP-EV3LKMN`, Windows 10.0 build
  **26100** (Windows 11 24H2).
- Whole negotiation, TCP connect to CredSSP verdict: under one second.

The server certificate was accepted silently; see FR-61 in step 5 for why and what M8
must do about it.

**Note:** 10.0.10.10 turned out to be the machine the operator sits at, so it cannot be
the subjective-test target from itself — an RDP logon as the console user would move the
session to the client and lock the console. Its credential-free results stand as a second
data point (they describe the host, not the session); the authenticated session runs
against 10.166.250.209 instead.

#### Results so far — 10.166.250.209 (Server 2019), credential-free probe (2026-09-09)

Same method, same fake username. This is the host the authenticated run and the subjective
test will use: a separate machine, reached over the real VPN path.

- Client offered `SSL | HYBRID | HYBRID_EX`; server selected **`HYBRID_EX`**. NLA enforced
  — `STATUS_LOGON_FAILURE (0xC000006D)` inside CredSSP, before any graphical logon.
- Response flags add `REDIRECTED_AUTHENTICATION_MODE_SUPPORTED` over the Win 11 box
  (Remote Credential Guard capable), plus `DYNVC_GFX_PROTOCOL_SUPPORTED`.
- NTLM CHALLENGE identifies it as `WIN-2QQ6BE2KRG9`, Windows 10.0 build **17763**
  (Server 2019).
- Reached over OpenVPN (client 10.19.0.2). The full multi-round-trip CredSSP exchange
  completed in **0.2 s over the VPN** — an encouraging early read on the row-4 latency
  question, though the authenticated session under load is the real test.

Still open, and needing the password: the authenticated session on this host — EGFX
capability confirmation, negotiated codec, the measurements, and the five feature checks.

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
| Certificate prompt on self-signed | FR-61 | **not testable with the stock viewer** | The library default is `DangerouslyAcceptInvalidCertificate` and the viewer does not override it, so the probe crossed a self-signed certificate with no prompt and no log line. The library does provide `CertificateValidation::Strict` and a `CertificateValidationCallback`, which is exactly the hook `CertPrompt` needs. M8: set `Strict` plus the callback and never rely on the default. |
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

Done: the Windows build; both credential-free probes (security layer, NLA enforcement, GFX
advertisement, host identity for hosts 1 and 2); the codec-landscape correction; ADR-2's
stale premise fixed. The target table rows for 10.0.10.10, 10.166.250.209, and the Ubuntu
box are filled. What remains needs the Administrator password and a human at the screen.

1. **Authenticated session** against 10.166.250.209 (the Win 11 box can't be its own
   target). Run it yourself so the password never reaches this transcript:

   ```powershell
   $env:RDP_HOSTNAME = "10.166.250.209:3389"
   $env:RDP_USERNAME = "Administrator"
   $env:RDP_PASSWORD = Read-Host -AsSecureString | ConvertFrom-SecureString -AsPlainText
   $env:IRONRDP_LOG  = "info,ironrdp_egfx=debug"
   ironrdp-viewer --log-file "$env:TEMP\spike-authed.log"
   ```

   Hand back `spike-authed.log` and the negotiated codec / EGFX line gets read out of it
   for the table — that part is mechanical.
2. **Steps 3–5** in the live window: the measurements, the five feature checks (FR-62
   under your keyboard layout especially), and the thirty minutes of real work that
   carries the most weight.
3. **Finish the Linux build**: the one `apt-get` line in step 1, then the rest is
   unprivileged and can be driven from here.
4. **Write the verdict.** Update ADR-2 from `Provisional` to `Accepted` (or `Superseded`).

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
