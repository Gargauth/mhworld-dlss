# Development

How to build, test, run a dev build and drive it, read the logs, and ship.
The design is in [ARCHITECTURE.md](ARCHITECTURE.md), the dev protocol in
[DEV-PROTOCOL.md](DEV-PROTOCOL.md), the reverse-engineering facts in
[re/](re/00-SUMMARY.md).

## Toolchain

Rust 1.98.1 (pinned in `rust-toolchain.toml` with `rustfmt`, `clippy` and the
`x86_64-pc-windows-gnu` target). The shipped binaries are always
`x86_64-pc-windows-gnu`: no Visual Studio is needed, on Windows or from
Linux.

| output | built from | installed as |
| --- | --- | --- |
| `mhw_dlss.dll` | `crates/mod` (`mhw-dlss`) | `<game>\mhw-dlss\mhw-dlss.dll` |
| `mhw-dlss-launcher.exe` | `crates/launcher` | `<game>\mhw-dlss\mhw-dlss-launcher.exe` |

### On Windows

Make the GNU toolchain the default host once, so that `1.98.1` from
`rust-toolchain.toml` resolves to it and `cargo build` targets
`x86_64-pc-windows-gnu`:

```powershell
rustup set default-host x86_64-pc-windows-gnu
git clone https://github.com/Gargauth/mhworld-dlss
cd mhworld-dlss
cargo build --release                       # target\release\
cargo build --profile dev-release --features dev --target-dir target\dev   # target\dev\dev-release\ (panics unwind)
.\deploy.ps1                                # build and copy into the game folder
.\deploy.ps1 -Dev                           # the same for the dev build
```

`deploy.ps1 -Game <folder>` if the game is not in Steam's default library.

### On Linux (cross build)

Needs the pinned toolchain (rustup reads `rust-toolchain.toml`), zig and
cargo-zigbuild; no root:

```sh
pip install --user ziglang==0.17.0 cargo-zigbuild==0.23.4   # or: uv tool install
tools/xbuild.sh release   # target/x86_64-pc-windows-gnu/release/
tools/xbuild.sh dev       # target/dev/x86_64-pc-windows-gnu/dev-release/
```

The dev build has its own target directory everywhere, so a dev DLL is
never picked up as the release one. Copy the two files into
`<game>\mhw-dlss\` under the names in the table above.

## Checks

What CI runs (`.github/workflows/ci.yml`), runnable locally:

```sh
cargo fmt --all --check
cargo clippy --workspace --exclude mhw-dev --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu --features dev -- -D warnings
cargo clippy --workspace --all-targets [--exclude mhw-dev | --features dev] -- -D warnings   # host cfg
cargo test --workspace --exclude mhw-dev    # pure logic, on any host
cargo test --workspace --features dev
cargo test --manifest-path tools/standin/Cargo.toml
python3 -m unittest discover -s tools/devctl
.github/scripts/check-dev-strings.sh target/x86_64-pc-windows-gnu/release target/dev/x86_64-pc-windows-gnu/dev-release
```

Clippy for the Windows target does not link, so it runs from Linux without
zig. Windows-only code is behind `#[cfg(windows)]`; the logic it needs lives
in `cfg`-free modules so that the tests run on Linux.

Scripts: every `*.ps1` and `*.cmd` must be plain ASCII (Windows PowerShell
5.1 reads a BOM-less script in the ANSI code page) and every `*.ps1` must
parse; CI checks both with `pwsh`. `install.ps1` and `fetch-nvidia.ps1` must
keep running on Windows PowerShell 5.1.

## The dev build

`--features dev` adds the `mhw-dev` crate: a TCP server through which a
client (a person or an agent) reads state, changes settings live, peeks and
pokes the engine's structs, captures frames and runs probes. Nothing of it
is in a release build: the code is behind `#[cfg(feature = "dev")]` and
`mhw-dev` is an optional dependency.

**The guarantee is checked on the binaries.** `mhw-dev` keeps the literal
`MHW_DLSS_DEV_BUILD` in every dev DLL. CI's
`.github/scripts/check-dev-strings.sh` fails if a release binary contains
that canary or any dev-only string (protocol method names and error codes,
`[Dev]`/`[Experimental]` keys), and also fails if the dev DLL lacks the
canary, so the check cannot pass by testing nothing. The dev archive is
named `-dev`, carries `DEV-BUILD.txt` and the devctl client, has its own
`SHA256SUMS-dev.txt`, and is never attached to a GitHub release.

### Settings

A dev build reads two more sections of `mhw-dlss.ini` (a release build
ignores them and says so once in its log):

```ini
[Dev]
Listen = 127.0.0.1:47800  ; 0.0.0.0:47800 to reach it from the LAN
TokenFile = dev.token     ; created with a random token on first start
LogTargets =              ; e.g. render=trace,fg=debug
DebugLayer = false        ; D3D12 debug layer (restart)
Dred = false              ; DRED breadcrumbs (restart)
CaptureDir = captures
DumpShaders = false       ; volumetric/fog shader bytecode to CaptureDir\shaders; compute overrides from its override\<hash>.hlsl (restart)

[Experimental]            ; defaults are the release behaviour
InputPath = Supersampling ; Supersampling | Pass
JitterScale = Display     ; Display | Render | Off
...
```

The full list with every value is in ARCHITECTURE.md (*config*).

### Connecting

`deploy.ps1 -Dev` prints the exact command. The token is created at the
first start of the game, in the mod folder (`TokenFile` is relative to it);
it is logged by path, never by value.

```sh
python tools/devctl/devctl.py --host 127.0.0.1 --token-file "<game>/mhw-dlss/dev.token" hello
python tools/devctl/devctl.py state
python tools/devctl/devctl.py set Experimental JitterScale Render
python tools/devctl/devctl.py log --level debug --targets render,fg
```

From another machine: set `[Dev] Listen = 0.0.0.0:47800`, restart the game,
allow TCP 47800 in the Windows firewall, copy `dev.token` to the client and
pass `--host <game PC>`. `$DEVCTL_HOST`, `$DEVCTL_TOKEN_FILE` save typing.
Everything devctl can do is in [tools/devctl/README.md](../tools/devctl/README.md).

### Probes: which question each answers

Each probe answers a question the static analysis left open
([re/00-SUMMARY.md](re/00-SUMMARY.md) §3) and decides a default in
`[Experimental]`.

| probe | question | decides |
| --- | --- | --- |
| `probe jitter` | Does the engine divide the jitter written to `sCamera+0x38/+0x3c` by the display or the render size? (E-jitter: `x / proj[8]` should come out near `2/Wd`.) | `JitterScale` |
| `probe reset` | When does the game set its history-reset flag (`taa+0x185`), and do camera cuts (cutscenes, teleports) show up in the camera metric without it? (E1) | `Reset`, `CUT_THRESHOLDS` |
| `probe mv` | Scale and sign of the game's dense velocity (`taa+0x1a8`) against the decoded `tGBuffer4` (`game_mv` vs `mv_raw`; run with `InputPath = Supersampling`, needs the temporal pass record of the same frame). (E2) | `InputPath = Supersampling` |
| `probe depth` | Does the supersampling path's depth copy match the depth the Pass path picks from the DSVs (`game_depth` vs `pass_depth`; `not_ready` if they are one texture or the pass did not run)? (E3) | the depth alias |
| `probe hdr` | Does the colour fed to DLSS exceed 1.0 (scene-referred), and how far? (E-HDR) | `DlssHdr` |
| `probe wait` | How often does the engine's wait helper time out, and is the latch already set? (E-wait) | `WaitLatch` |
| `probe modules` | Which modules load before the D3D12 device exists? (E-proxy, answered: only the game's own `steam_api64.dll` and `amd_ags_x64.dll` come from its folder in time, [re/ReLoadInitDx11.md](re/ReLoadInitDx11.md) §2.7) | a re-check after game or Steam updates |
| `trace` | Per-frame order and timing of camera, slot, evaluate, tags, markers, present. | pacing and frame-token bugs |
| `capture` | What the inputs actually look like (colour, depth, motion vectors, DLSS output, HUD-less). | visual checks, regressions |

## Logging

`mhw-core::log`; one file, `mhw-dlss.log` in the mod folder (the previous
run kept as `mhw-dlss.previous.log`, 32 MiB cap per run). The launcher writes
`mhw-dlss-launcher.log` in the same format.

```text
2026-10-11T15:04:05.123 I [render] feature created w=1920 h=1080
```

UTC milliseconds, level letter, target, message, structured fields
(`log::info!(target: "render", w = 1920, h = 1080; "feature created")`). The
user profile path is replaced with `%USERPROFILE%` in every line.

- **Levels**: `Error` something failed and a feature is off; `Warn` degraded
  or surprising; `Info` once-per-session facts (versions, paths, decisions);
  `Debug` per-event detail; `Trace` per-frame detail. `[General] LogLevel`
  sets the global level (live).
- **Targets** (subsystems, not module paths): `boot gate engine jitter hijack
  tune render taa inputs dlss ngx fg sl reflex swap hudless dev perf config
  log crash`; the launcher logs as `launcher`. Per-target levels in a dev
  build: `[Dev] LogTargets = render=trace,fg=debug` (live: edit the file,
  or `devctl set Dev LogTargets render=trace`). `devctl log --level …
  --targets …` only filters what it streams: the stream carries records
  that already passed the logger's levels, so raise the target's level
  first, then subscribe.
- **Per-frame code** never logs above `Debug` except through the rate
  helpers `log_once!`, `log_every!`, `log_changed!`.

## The standin

`tools/standin` (not a workspace member) replays the engine's renderer
start-up so that the mod and companion layers (ReShade and its add-ons,
Streamline) can be checked without starting the game: system-path loads,
`GetProcAddress`, a pipeline that reflects like the temporal pass, the
game's swapchain flags, optionally one DLSS frame. It judges the mod's log;
the lines it relies on are listed in `tools/standin/src/verdict.rs` and must
stay stable. Usage is in the header of `tools/standin/src/main.rs`.

```sh
cargo zigbuild --release --target x86_64-pc-windows-gnu --manifest-path tools/standin/Cargo.toml
```

## Commits and releases

- One concern per commit; the subject is imperative and at most 72
  characters; the body says why. Pure logic gets tests first (contract,
  tests, code).
- Before pushing: the checks above, clean.
- **Release**: set `version` in the root `Cargo.toml` (`[workspace.package]`)
  and commit; tag `vX.Y.Z` on that commit and push the tag. The release
  workflow checks that tag and version agree, cross-builds both flavours
  from the tag, and creates a **draft** release with
  `mhworld-dlss-vX.Y.Z.zip` and `SHA256SUMS.txt`. Read the draft, then
  publish it by hand. Assets are never replaced once attached. The dev
  archive stays an artifact of the workflow run.
- `install.ps1` installs the latest published release, so publishing is the
  moment users get it.
