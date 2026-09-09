# SPIKE-RDP.md

**Status: NOT STARTED**

This spike resolves ADR-2, which is `Provisional`. It is Milestone M0 and it blocks
everything else. Budget one day.

---

## What is being decided

Whether `ironrdp` is good enough to be the RDP backend, or whether we take FFI bindings to
`libfreerdp` instead.

The concern is specific. `ironrdp` supports raw bitmap, interleaved RLE, RDP 6.0 bitmap
compression, and RemoteFX. It does not implement the full H.264/AVC444 graphics pipeline
that current Windows hosts prefer. A modern host will negotiate down to something
`ironrdp` supports, so the connection succeeds — the question is whether what you get is
good enough to work in for a full day, against hosts configured the way yours actually
are.

This is an empirical question. It cannot be answered from documentation and it must not be
answered by assumption in either direction.

---

## Why it runs before anything else

If the answer is `libfreerdp`, the consequences reach the toolchain and the packaging
story: a C build dependency on both platforms, `bindgen`, MSVC linkage on Windows, and the
loss of the single-self-contained-binary property in NFR-2. Discovering that at Milestone
8 is expensive. Discovering it on day one costs nothing.

Nothing built during this spike is kept. Do not build reusable abstractions here.

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
LAN result tells you nothing about the VPN case, and the codec gap is exactly where
bandwidth efficiency matters most.

---

## Procedure

### 1. Build the upstream viewer

Clone `Devolutions/IronRDP` and build `ironrdp-viewer`, the portable client binary. Do not
write your own harness — the upstream client already exercises the full connect path and
is what its maintainers test.

Build it on Windows too, with the MSVC toolchain, not only on Linux. CredSSP behaviour
differs across platforms and the Windows path is the one your users will be on.

### 2. Connect to each target

For each host in the table, record:

- Does the connection establish at all?
- Which security layer was negotiated?
- Which codec was negotiated? (Enable `ironrdp` trace logging; do not guess.)
- Did the host request something unsupported, and did the fallback happen cleanly or
  produce visible corruption?

### 3. Measure

| Metric | How | Bar |
|---|---|---|
| Connect time to desktop | Stopwatch, three attempts | Under 5 s on LAN |
| Text legibility | Open Notepad and a PowerShell window at 100% and 150% scaling | No visible ringing or smearing on small text |
| Scroll smoothness | Scroll a long document continuously | No tearing; no multi-second catch-up |
| Window drag latency | Drag a window across the desktop | Under ~150 ms perceived |
| Bandwidth at idle | OS network counters, 60 s idle session | Note it; compare to `xfreerdp` |
| Bandwidth under scroll | Same, during continuous scroll | Note it; compare to `xfreerdp` |

Run the same measurements with `xfreerdp` against the same hosts on the same network.
The comparison is the point — absolute numbers without a FreeRDP baseline do not decide
anything.

### 4. The subjective test

Do thirty minutes of real work in an `ironrdp` session. Not a benchmark — actual work.
Edit a file, navigate a file manager, use a terminal, read something. This catches what
the metrics miss, and it is the test that should carry the most weight.

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

---

## Decision rule

Write the verdict below. State it plainly; do not hedge it into uselessness.

**Go with `ironrdp` if:** it connects to every target, NLA works on both platforms, the
five feature checks pass, and thirty minutes of real work was not annoying.

**Go with `libfreerdp` FFI if:** any target fails to connect, NLA fails on Windows, keyboard
handling is wrong under your layout, or the subjective test was frustrating.

**Ambiguous result:** treat as a no-go for the *default*, but structure the code to support
both backends behind the trait, selected by a Cargo feature. Ship FreeRDP as the default
and keep `ironrdp` as the pure-Rust option. This costs one extra crate and is the right
answer when the result depends on which host you connect to.

Bandwidth being somewhat worse than FreeRDP is not by itself a no-go. Bandwidth being bad
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
