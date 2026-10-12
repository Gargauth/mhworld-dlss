# mhworld-dlss

DLSS 4 upscaling, DLAA and DLSS frame generation for **Monster Hunter: World**
on PC (Steam, DirectX 12).

The game shipped in 2018 with DLSS 1, which looks soft and smeary and which
current graphics cards no longer run. This mod takes over the game's own DLSS
path and runs NVIDIA's current DLSS model in its place, and can add generated
frames on top (2x to 4x). No game files are changed: clear one Steam launch
option and the game is back to normal.

Unofficial, not made or endorsed by Capcom or NVIDIA. Free software under
GPL-3.0; it contains no NVIDIA code or files.

## Features

- **DLAA**: full resolution, DLSS only anti-aliases.
- **Quality / Balanced / Performance / Custom** (default Quality): the game
  renders fewer pixels and DLSS 4 reconstructs the rest. The interface is
  always drawn at full resolution.
- **Frame generation** 2x, 3x or 4x through NVIDIA Streamline, with Reflex
  low latency. Off by default.
- **DLSS model presets** `J`, `K`, `L`, `M`, or let the driver choose.
- **Sharpening** after DLSS (AMD's RCAS), applied before the interface, so
  text and icons stay clean. Off by default.
- **Full-resolution volumetric fog** (optional): no more stair-stepped edges
  where objects meet fog.
- **Settings apply while you play**: edit `mhw-dlss.ini`, or use the
  mod's page in ReShade's overlay.
- **HDR**: works with the game's own HDR output and with the RenoDX HDR
  add-on, frame generation included.
- **Safe after game updates**: the mod checks the game's code before it
  touches anything. If the game changed, it stands down, says why in its log,
  and the game runs unmodified.
- **Plays well with others**: ReShade, RenoDX, OptiScaler and Stracker's
  Loader, see [Compatibility](#compatibility).
- **One-line installer** that also fetches NVIDIA's libraries and, if you
  want, ReShade, RenoDX and OptiScaler.

## Requirements

| | needed |
| --- | --- |
| Game | Monster Hunter: World, a legitimate Steam install (made for build 15539686; a later build works as long as the mod still recognises the game's code) |
| Game settings | DirectX 12 on, Anti-Aliasing set to **TAA**, the game's own "NVIDIA DLSS" option **off** |
| Graphics card | NVIDIA RTX. Frame generation: RTX 40 or 50 series; NVIDIA limits 3x and 4x to the 50 series |
| Driver | a recent one |
| System | Windows 10 or 11, 64-bit |

## Compatibility

| tool | works | how |
| --- | --- | --- |
| [ReShade](https://reshade.me) | yes, with and without frame generation | loaded by the mod, not by ReShade's installer; the installer can set it up |
| [RenoDX](https://github.com/clshortfuse/renodx) (HDR) | yes, with frame generation | needs ReShade with add-on support; the installer can set it up |
| Game's own HDR | yes | turn HDR on in Windows and in the game |
| [OptiScaler](https://github.com/optiscaler/OptiScaler) | yes, official 0.9.4 | not needed for anything the mod does; the installer can set it up with the settings it needs |
| [Stracker's Loader](https://www.nexusmods.com/monsterhunterworld/mods/1982) and its plugins | yes, 4.0 with QuestLoader and MonsterLoader | install it as its own instructions say; nothing else to do |
| The game's DLSS 1 option | no | leave it off; the mod replaces it |
| DirectX 11 mode | no | the mod stands down and the game runs unmodified |

Details for each tool are under [Companion tools](#companion-tools).

## Install

**First, in the game:** in the display options turn DirectX 12 on, set
Anti-Aliasing to TAA, leave "NVIDIA DLSS" off, and quit the game.

### Option 1: one line (recommended)

1. Open PowerShell (Start, type `PowerShell`, Enter) and paste:

   ```powershell
   irm https://raw.githubusercontent.com/Gargauth/mhworld-dlss/main/install.ps1 | iex
   ```

   The script ([read it first](install.ps1)) finds the game through Steam,
   downloads the latest release, checks it against the release's SHA-256
   sums, copies it into a folder `mhw-dlss` inside the game folder, and
   fetches NVIDIA's libraries. It asks whether to turn frame generation on
   and whether to add ReShade, the RenoDX HDR add-on or OptiScaler; Enter
   answers no. It needs no administrator rights.

2. The script ends by printing the Steam launch option. Copy it into Steam:
   Library, right-click Monster Hunter: World, Properties, General,
   **Launch Options**, quotes included. Your path may differ:

   ```text
   "C:\Program Files (x86)\Steam\steamapps\common\Monster Hunter World\mhw-dlss\mhw-dlss-launcher.exe" %command%
   ```

3. Start the game from Steam as usual.

Running the same line again updates the mod and keeps your settings.

### Option 2: by hand

1. Download `mhworld-dlss-vX.Y.Z.zip` from
   [Releases](https://github.com/Gargauth/mhworld-dlss/releases) and extract
   it into the game folder (Steam: right-click the game, Manage, Browse local
   files). It adds one folder, `mhw-dlss`, and nothing else. Archives ending
   in `-dev` are developer builds; do not use them to play.
2. **Double-click `mhw-dlss\fetch-nvidia.cmd`.** The mod does nothing without
   NVIDIA's libraries and is not allowed to ship them. The script downloads
   NVIDIA's official Streamline SDK 2.14.1 (about 276 MB) from
   <https://github.com/NVIDIA-RTX/Streamline/releases>, checks its SHA-256
   and NVIDIA's signature on every file, and copies seven libraries into
   `mhw-dlss`. Running it means you accept NVIDIA's licence for them.
3. Set the Steam launch option as in Option 1, with your own game folder.
4. Start the game from Steam as usual.

### Did it work?

The launcher shows no window: it starts the game with the mod inside and
exits. After loading a save, `mhw-dlss\mhw-dlss.log` has a line like

```text
2026-10-11T15:04:05.123 I [dlss] CreateFeature render=... display=... ... status="0x00000001 Success" frame=...
```

If it does not, see [Troubleshooting](#troubleshooting).

## Settings

The first start writes `mhw-dlss\mhw-dlss.ini`, every key explained in it.
It is read again within a second of being saved: keys marked *live* apply
while you play, the others at the next start. Names are not case-sensitive;
`true/false` may be written `on/off` or `1/0`.

With ReShade loaded, the same settings are also in ReShade's overlay (Home
key), in the mod's section of the **Add-ons** tab. The page writes
`mhw-dlss.ini`, so the file stays the one place your settings live.

| section | key | default | what it does |
| --- | --- | --- | --- |
| `[General]` | `Enabled` | `true` | `false` turns the whole mod off. Restart. |
| | `LogLevel` | `Info` | How much goes into `mhw-dlss.log`: `Error`, `Warn`, `Info`, `Debug`, `Trace`. Live. |
| `[Upscaling]` | `Mode` | `Quality` | `DLAA` (full resolution, anti-aliasing only), `Quality` (0.667 per axis), `Balanced` (0.58), `Performance` (0.5), or `Custom`. Live. |
| | `RenderScale` | `0.667` | With `Mode = Custom`: the fraction of the display resolution rendered, `0.50` to `1.00`. Live. |
| | `Preset` | `Default` | The DLSS model: `Default` lets the driver choose; `J`, `K`, `L`, `M`. Live. |
| | `Sharpness` | `0.0` | Sharpening after DLSS and before the interface is drawn, `0.0` (off) to `1.0`. Live. |
| | `DownsampleVolume` | `true` | The game draws its volumetric fog from a blurred half-resolution copy, which leaves stair-stepped edges where objects meet fog. `false` draws the fog at full resolution so DLSS smooths those edges, at a small cost in performance. Restart. |
| `[FrameGeneration]` | `Enabled` | `false` | DLSS frame generation. Restart. |
| | `Multiplier` | `2` | Frames shown per rendered frame: `2`, `3` or `4`. Live. |
| | `ReflexBoost` | `false` | Ask Reflex to keep the clocks up as well, at the cost of power. Reflex itself is always on with frame generation. Live. |

The game's own Image Quality option has no effect while the mod runs; `Mode`
takes its place. Texture sharpness (mip bias), the jitter and the reset of
DLSS's history on camera cuts are set automatically.

<details>
<summary><b>Coming from 0.2</b></summary>

0.2 wrote the same file name with different keys (`render_scale = 0.667`,
`fg = 1`, no sections). 0.3 does not read them. The installer moves such a
file to `mhw-dlss.0.2.ini` and puts the new default in place; by hand,
delete or rename the old file and the mod writes a new one. Then carry your
values over:

| 0.2 | 0.3 |
| --- | --- |
| `render_scale = 1.0 / 0.667 / 0.58 / 0.5` | `[Upscaling] Mode = DLAA / Quality / Balanced / Performance` |
| `render_scale = <other>` | `[Upscaling] Mode = Custom`, `RenderScale = <other>` |
| `sharpen` | `[Upscaling] Sharpness` |
| `dlss_preset = 0 / 10 / 11 / 12 / 13` | `[Upscaling] Preset = Default / J / K / L / M` |
| `fg = 0 / 1` | `[FrameGeneration] Enabled = false / true` |
| `fg_multiplier` | `[FrameGeneration] Multiplier` |
| `reflex_boost` | `[FrameGeneration] ReflexBoost` |
| `mip_bias`, `mip_bias_extra`, `undither`, `fg_interpolate`, `fg_hudless`, `expert_*`, `diag_*` | gone: fixed defaults now |

</details>

## Frame generation

- Needs an RTX 40 or 50 series card; NVIDIA limits 3x and 4x to the 50 series.
- Needs NVIDIA's libraries from `fetch-nvidia.cmd` (the one-line installer
  fetches them).
- Set `[FrameGeneration] Enabled = true`, then restart the game. The
  multiplier can be changed while you play.
- Generation pauses while the game window is not focused.
- Works only when the game is started through the launcher: the swapchain it
  needs is created once, early, and never again.

## Companion tools

None of these is included; each has its own download and licence. The
one-line installer offers to fetch ReShade, RenoDX and OptiScaler from their
authors in a version the mod was verified with, and sets them up. Step-by-step
instructions for doing it by hand: [docs/companions.md](docs/companions.md).

**How they are loaded.** Tools like ReShade are normally installed as
`dxgi.dll` or `dinput8.dll` beside the game. In this game none of those names
is loaded early enough, so **do not run ReShade's installer or OptiScaler's
`setup_windows.bat` against the game**. Instead the launcher loads them from
`mhw-dlss\mhw-dlss-loader.ini`, one library per line:

```text
mhw-dlss.dll
ReShade64.dll  renderer
OptiScaler.dll  renderer
```

The libraries go into the `mhw-dlss` folder; the word `renderer` is required.

### ReShade

Verified with ReShade 6.8.0 with add-on support. Take `ReShade64.dll` out of
the setup program (it is also an archive; 7-Zip opens it), put it in
`mhw-dlss` and add `ReShade64.dll  renderer` to the chain file. The overlay
opens with Home. ReShade keeps its own files (`ReShade.ini`, presets, shaders,
add-ons) beside the game's executable. With frame generation on, effects are
applied to generated frames too.

### HDR and RenoDX

The game's own HDR output works with the mod, frame generation included: turn
HDR on in Windows (Settings, System, Display) and in the game's display
options. For better HDR, the [RenoDX](https://github.com/clshortfuse/renodx)
add-on replaces the game's tone mapping: set up ReShade with add-on support,
then put `renodx-mhworld.addon64` beside the game's executable. Nothing goes
into the chain file for it.

### OptiScaler

Not needed: the mod already makes the DLSS calls and drives frame generation
itself. If you want it anyway (its overlay, its upscaler options), use the
official 0.9.4 release, put `OptiScaler.dll` and `OptiScaler.ini` in
`mhw-dlss` (not the game folder), add `OptiScaler.dll  renderer` to the chain
file, and set the seven keys listed in
[docs/companions.md](docs/companions.md#optiscaler). Its one-off "Late
Streamline hook detected" notice is expected and harmless. Third-party forks
are not supported.

### Stracker's Loader

Stracker's Loader 4.0 and its QuestLoader and MonsterLoader plugins work
beside the mod, frame generation included. Install it as its own instructions
say (beside the game's executable, not in `mhw-dlss`); nothing goes into the
chain file. The mod itself is not a Stracker's Loader plugin and must not be
put in `nativePC\plugins`. A plugin that changes the game's renderer could
conflict with the mod.

## Troubleshooting

Two logs are written into `mhw-dlss` at every start: `mhw-dlss-launcher.log`
and `mhw-dlss.log`; the run before is kept as `...previous.log`. Lines read
`time level [subsystem] message`; `W` and `E` lines say what went wrong.

| what you see | what it means | what to do |
| --- | --- | --- |
| The game runs as before and `mhw-dlss.log` is missing or old | the game was not started through the launcher | check the launch option: full path in quotes, a space, `%command%`; `mhw-dlss-launcher.log` says what the launcher did |
| A `[gate]` line saying the mod stands down | it runs in DirectX 11, or a game update changed code or data the mod relies on (the line names what no longer matches) | turn DirectX 12 on in the game; after a game update, wait for a mod update (the game runs unmodified meanwhile) |
| `[ngx]` or `[dlss]` warnings about `nvngx_dlss.dll` | NVIDIA's libraries are missing | run `fetch-nvidia.cmd` in `mhw-dlss`, start again |
| No `[dlss] CreateFeature` line after loading a save | Anti-Aliasing is not TAA | set Anti-Aliasing to TAA |
| `[config]` lines naming unknown keys | the settings file is from 0.2 | see *Coming from 0.2* under [Settings](#settings) |
| Frame generation on, frame rate unchanged | `Enabled` changed without a restart, the window is not focused, or the card does not support it | restart; read the `[fg]`, `[sl]` and `[swap]` lines |
| No mod page in ReShade's overlay | the ReShade build has no add-on interface, or a version the page does not know | the `settings page:` line in `mhw-dlss.log` says which; edit `mhw-dlss.ini` instead |
| The game starts in DirectX 11 after a crash | the game switches itself to DirectX 11 after a graphics crash | turn DirectX 12 back on |
| A launcher message that `mhw-dlss.dll` is not there | antivirus software removed it | restore it or extract the release again |
| Steam does not start the game after uninstalling | the launch option still names the launcher | clear the launch option |

**Reporting a problem:** open an
[issue](https://github.com/Gargauth/mhworld-dlss/issues) and attach
`mhw-dlss.log`, `mhw-dlss-launcher.log` (the `previous` ones if you have
restarted since), `mhw-dlss.ini`, `mhw-dlss-loader.ini`, and with frame
generation on `sl.log`. The logs replace your Windows profile path with
`%USERPROFILE%`; look through them before posting.

## Before you use it

The launcher starts the game suspended and loads a DLL into it, which some
antivirus software objects to; every release has a `SHA256SUMS.txt`. The mod
has been used in normal online play and the game has no known anti-cheat, but
it is an unofficial modification: use it at your own risk. The mod itself has
no networking code; with frame generation on, NVIDIA's Streamline may fetch
updates of its own components.

## Uninstall

1. Clear the Steam launch option.
2. Delete the `mhw-dlss` folder from the game folder. That is the whole mod.

Or let the installer do step 2:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gargauth/mhworld-dlss/main/install.ps1))) -Uninstall
```

It removes what it installed (recorded in `mhw-dlss\mhw-dlss-installed.txt`),
the logs and the download caches, and keeps your settings (`mhw-dlss.ini`,
`mhw-dlss-loader.ini`, `mhw-dlss.0.2.ini`) and anything it did not put there.
`-Purge` removes the whole folder. ReShade's own files beside the game's
executable (`ReShade.ini`, presets, add-ons) are yours to remove.

## How it works

In short: the game still believes it runs its own DLSS 1, but those calls are
taken over and current DLSS runs instead, fed with the game's own depth,
motion vectors and camera jitter. Frame generation goes through NVIDIA
Streamline on the game's swapchain. Nothing is written to the game's files;
all changes live in memory while the game runs. The design is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Building from source

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md): building on Windows and
Linux, the dev build, tests and the release process.

## What this project will not do

- Support a copy of the game that is not a legitimate Steam install.
- Ship NVIDIA's files, or patch them.
- Write to the NVIDIA driver's registry settings or application profiles.
- Present itself to NVIDIA's software as a different application.

## Licence

Copyright (C) 2026 Gargauth. GPL-3.0-only, without any warranty: see
[LICENSE](LICENSE), and [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for
what it is built from and the terms of NVIDIA's files. Thanks to the authors
of [ReShade](https://reshade.me),
[RenoDX](https://github.com/clshortfuse/renodx) and
[OptiScaler](https://github.com/optiscaler/OptiScaler), and to NVIDIA for
publishing Streamline. NVIDIA, DLSS, Reflex and Streamline are trademarks of
NVIDIA Corporation; Monster Hunter: World is a trademark of CAPCOM CO., LTD.
This project is not affiliated with either.
