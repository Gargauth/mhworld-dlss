# mhworld-dlss

DLSS 4 upscaling, DLAA and DLSS frame generation for **Monster Hunter: World**
on PC (Steam, DirectX 12). The game shipped in 2018 with DLSS 1, which
current graphics cards no longer run. This mod replaces the game's temporal
anti-aliasing pass with NVIDIA's current DLSS model and can add generated
frames (2x to 4x) on top.

It is an unofficial mod, not made or endorsed by Capcom or NVIDIA. It is free
software under GPL-3.0 and contains no NVIDIA code or files.

## Quick start

You need the Steam version of the game, an NVIDIA RTX graphics card with a
recent driver, and Windows 10 or 11. Details are under
[Requirements](#requirements).

**First, in the game:** in the display options turn DirectX 12 on and set
Anti-Aliasing to TAA. Leave the game's own "NVIDIA DLSS" option off. Quit the
game.

### Route 1: one line

1. Open PowerShell (press Start, type `PowerShell`, press Enter) and paste:

   ```powershell
   irm https://raw.githubusercontent.com/Gargauth/mhworld-dlss/main/install.ps1 | iex
   ```

   This downloads a script from this repository and runs it. You can
   [read it first](install.ps1); Route 2 does the same things by hand. The
   script finds the game through Steam, downloads the latest release of the
   mod and checks it against the release's SHA-256 sums, copies it into a
   folder `mhw-dlss` inside the game folder, and then fetches NVIDIA's
   libraries (see step 2 of Route 2 for what those are and where they come
   from). On the way it asks whether to turn frame generation on and whether
   to install ReShade, the RenoDX HDR add-on or OptiScaler; Enter answers no
   to each. It needs no administrator rights.

2. The script ends by printing a line like the one below. Copy it. In Steam:
   Library, right-click Monster Hunter: World, Properties, General, and paste
   it into **Launch Options**, quotes included. Your path may differ.

   ```text
   "C:\Program Files (x86)\Steam\steamapps\common\Monster Hunter World\mhw-dlss\mhw-dlss-launcher.exe" %command%
   ```

3. Start the game from Steam as usual.

### Route 2: by hand

1. Download `mhworld-dlss-vX.Y.Z.zip` from
   [Releases](https://github.com/Gargauth/mhworld-dlss/releases) and extract
   it into the game folder (Steam: right-click the game, Manage, Browse local
   files). It adds one folder, `mhw-dlss`, and touches nothing else.

2. **Fetch NVIDIA's libraries: open the `mhw-dlss` folder and double-click
   `fetch-nvidia.cmd`.** The mod does nothing without them, and this project
   is not allowed to ship them. The script downloads NVIDIA's official
   Streamline SDK 2.14.1 archive (about 276 MB) from
   <https://github.com/NVIDIA-RTX/Streamline/releases>, checks its SHA-256
   and NVIDIA's signature on every file, and copies seven libraries into the
   `mhw-dlss` folder. Two of them, `sl.interposer.dll` and `nvngx_dlss.dll`,
   do not come with the graphics driver at all, which is why this step
   exists. Running it means you accept NVIDIA's licence for those files; the
   licence texts are copied next to them.

3. Set the Steam launch option as in step 2 of Route 1, with the path of
   your own game folder.

4. Start the game from Steam as usual.

### Did it work?

The launcher shows no window: it starts the game with the mod inside and
exits. Open `mhw-dlss\mhw-dlss.log` after loading a save. A working session
has these two lines, with your own resolutions:

```text
[hij] the game's DLSS path is ours
[dls] CreateFeature 2561x1440 -> 3840x2160 ... -> 0x00000001
```

If not, see [Troubleshooting](#troubleshooting). To get the unmodified game
back at any time, clear the launch option.

Everything the mod installs or writes is in that one folder: the launcher,
the mod, NVIDIA's libraries, the settings and the logs. The one exception is
not the mod's: ReShade, if you add it, keeps its own settings, log and
add-ons beside the game's executable.

## What it does

* **Upscaling.** The game renders the scene at a fraction of your display
  resolution and DLSS 4 reconstructs the rest; the interface is drawn on top
  at full resolution. The default fraction is 0.667 per axis: about 2560x1440
  for a 3840x2160 display.
* **DLAA.** At a fraction of 1.00 the scene is rendered at full resolution
  and DLSS only anti-aliases it.
* **Frame generation**, optional and off by default: 2, 3 or 4 presented
  frames per rendered frame through NVIDIA Streamline, with Reflex low
  latency switched on alongside.

It has been verified in the game on one machine: an RTX 5070 Ti, driver
616.92, a 3840x2160 HDR display, Windows 11. Upscaling, DLAA and frame
generation at 2x, 3x and 4x all work there. Other graphics cards and 1080p or
1440p displays are untested. The mod's own work costs about 3 to 4 ms of CPU
time per frame, measured in a scene limited by the CPU.

## Before you use it

The launcher starts the game suspended, loads the mod's DLL into it and then
lets it run; the mod patches the game's rendering code in memory. Antivirus
software may object to a program that does this. Every release carries a
`SHA256SUMS.txt` so you can check what you downloaded.

Online play: the mod has been used during normal online play on the Steam
version, and the game has no anti-cheat that we know of. It is still an
unofficial mod that injects a DLL, and nobody can promise how a publisher
treats modified clients. You use it at your own risk.

Network: the mod itself contains no networking code. With frame generation
on, NVIDIA's Streamline may check for and download updated components of its
own (its over-the-air update mechanism). The installer and `fetch-nvidia`
download only what is described above.

## Requirements

| | needed | verified with |
| --- | --- | --- |
| Game | Monster Hunter: World, a legitimate Steam install | Steam build 15539686 |
| Game settings | DirectX 12 on; Anti-Aliasing set to TAA | `DirectX12Enable=On`, `Anti-Aliasing=TAA` in `graphics_option.ini` |
| Graphics card | NVIDIA RTX. Frame generation: RTX 40 or 50 series; NVIDIA limits 3x and 4x to the 50 series | RTX 5070 Ti |
| Driver | a recent one | 616.92 |
| System | Windows 10 or 11, 64-bit | Windows 11 |

The mod replaces the game's temporal anti-aliasing pass, so with
Anti-Aliasing off it has nothing to replace and idles. The game's own
"NVIDIA DLSS" option is not needed and should stay off. The game's Image
Quality option has no effect while the mod is loaded; `render_scale` takes
its place.

After a game update the mod checks 16 places in the game's code before it
touches anything. If any differs, it installs nothing, loads nothing of
NVIDIA's, says why in one line of its log, and the game runs unmodified until
the mod is updated. The same happens in DirectX 11 mode.

## Settings

The first start writes `mhw-dlss\mhw-dlss.ini`, with every key explained. It
is read again within a second of being saved, so most settings can be changed
while the game runs. These are the ones that matter:

| key | default | what it does |
| --- | --- | --- |
| `render_scale` | `0.667` | Fraction of the display resolution the scene is rendered at, any value from `0.50` to `1.00`. `1.00` is DLAA, `0.667` is DLSS Quality, `0.58` Balanced, `0.50` Performance. Applies while playing. |
| `mip_bias` | `0` | `1` sharpens texture sampling to suit the render scale; `0` leaves it as the game has it. Fixed when the game starts, for the render scale set then: restart after changing either. |
| `fg` | `0` | `1` turns frame generation on. Decided when the game starts: restart after changing it. |
| `fg_multiplier` | `2` | Frames shown per rendered frame: `2`, `3` or `4`. Applies while playing. |
| `fg_interpolate` | `1` | In a session started with `fg = 1`, `0` switches generated frames off without a restart. |
| `reflex_boost` | `0` | `1` asks Reflex to keep the CPU and GPU clocks up as well, at the cost of power. Reflex itself is always on with frame generation. |

Frame generation stops while the game window is not focused. The file has a
few more picture and frame generation keys, each explained there, and a
block of diagnostics that is only for bug reports.

With ReShade loaded (below), the same settings are on a page in ReShade's
overlay: open the overlay, go to the **Add-ons** tab and expand `mhw-dlss`.
The page edits the same ini file. Its render scale slider snaps to 0.5,
0.58, 0.667, 0.75, 0.85 and 1.0; its frame generation box writes `fg` and
says when a restart is needed.

## Companion tools

All three are optional, none is included here, and each was verified in the
game together with this mod. The one-line installer offers to download and
set up each of them from its authors; what follows is the same by hand. They
are wired in through one file,
`mhw-dlss\mhw-dlss-loader.ini`, which the launcher writes on first start. It
lists what is loaded into the game, one library per line, looked for in the
`mhw-dlss` folder:

```text
mhw-dlss.dll
ReShade64.dll  renderer
OptiScaler.dll  renderer
```

The word `renderer` is required for both tools. The usual way of installing
them, renaming the DLL to `dxgi.dll`, `dinput8.dll` or similar, does not work
in this game: it loads none of those names early enough. Do not run their
installers or setup scripts against the game.

| tool | what it adds | in short |
| --- | --- | --- |
| [ReShade](https://reshade.me) 6.8.0, the build "with full add-on support" | post-processing effects, and this mod's settings page | put `ReShade64.dll` in the `mhw-dlss` folder, add the `ReShade64.dll  renderer` line |
| [RenoDX](https://github.com/clshortfuse/renodx) add-on for this game | better HDR | needs ReShade as above; put `renodx-mhworld.addon64` beside the game's executable, where ReShade looks for add-ons; turn on HDR in Windows and in the game |
| [OptiScaler](https://github.com/optiscaler/OptiScaler) 0.9.4 | its own overlay and upscaler options | put `OptiScaler.dll` and `OptiScaler.ini` in the `mhw-dlss` folder, not in the game folder; add the `OptiScaler.dll  renderer` line; change seven settings in `OptiScaler.ini` |

The exact steps, including where ReShade keeps its own files and the
OptiScaler settings, are in [docs/companions.md](docs/companions.md).

## Troubleshooting

Two logs are written into the `mhw-dlss` folder at every start:
`mhw-dlss-launcher.log` by the launcher and `mhw-dlss.log` by the mod. The
run before is kept beside each as `...previous.log`.

`mhw-dlss.log` opens with the mod's version, the path of the game executable
(`host`), of the log itself and of the mod (`self`). A line starting `[bld]`
then says whether the game is the build the mod knows, and a line starting
`[dls]` says whether DLSS can be used, or why not.

| what you see | what it means | what to do |
| --- | --- | --- |
| The game runs as before and `mhw-dlss.log` is missing or old | the game was not started through the launcher | check the Steam launch option: full path in quotes, then a space and `%command%`. `mhw-dlss-launcher.log` says what the launcher did |
| Log: `[dls] DLSS is not used in this session: ... nvngx_dlss.dll is not there` | NVIDIA's libraries are missing | double-click `fetch-nvidia.cmd` in the `mhw-dlss` folder, then start the game again |
| Log: `[bld] standing down: ...` | the game is not the build the mod knows (after a game update), or it is set to DirectX 11; the line says which | for DirectX 11, turn DirectX 12 on in the game. After a game update, wait for an update of the mod; the game runs unmodified meanwhile |
| Log: `[hij] no temporal pass ... anti-aliasing is off` | Anti-Aliasing is not TAA | set Anti-Aliasing to TAA in the game |
| Frame generation is on but the frame rate does not rise | `fg` was changed without a restart, the window is not focused, or the graphics card does not support it | restart the game; look at the `[fg ]` and `[sl ]` lines of the log |
| A message from the launcher that `mhw-dlss.dll` is not there | antivirus software removed or quarantined it | restore it or extract the release again, and add an exception if you trust it |
| Steam will not start the game after uninstalling | the launch option still names the launcher | clear the launch option |

**Reporting a problem.** Open an issue and attach, from the `mhw-dlss`
folder, `mhw-dlss.log` and `mhw-dlss-launcher.log` (the `previous` ones
instead if you have started the game again since), `mhw-dlss.ini` and
`mhw-dlss-loader.ini`, and with frame generation on also `sl.log`. Say which
graphics card, driver version and display resolution you have, and which
companion tools are loaded. The logs replace your Windows profile path with
`%USERPROFILE%`, but look through them before posting.

## Uninstall

1. Clear the Steam launch option. The game will not start through a launcher
   that is gone.
2. Delete the `mhw-dlss` folder from the game folder. That is the whole mod:
   it installed and changed nothing else.

Optionally delete `%LOCALAPPDATA%\mhw-dlss`, where `fetch-nvidia` keeps the
276 MB archive it downloaded (the one-line installer keeps it in
`%TEMP%\mhw-dlss` instead).

If you added ReShade: it keeps its own files beside the game's executable,
not in the `mhw-dlss` folder. `ReShade.ini`, `ReShade.log`, its presets and
shaders, and add-ons such as `renodx-mhworld.addon64` are there, and are
yours to remove.

The installer can do step 2 for you:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gargauth/mhworld-dlss/main/install.ps1))) -Uninstall
```

It removes what it installed, which it recorded in
`mhw-dlss\mhw-dlss-installed.txt`: the mod, NVIDIA's libraries, any companion
tool it downloaded (the RenoDX add-on beside the executable included) and
the lines it added to the chain file. It also removes the logs and both cache
folders. It keeps `mhw-dlss.ini`, `mhw-dlss-loader.ini` and anything it did
not put there, and lists what it kept. Add `-Purge` to remove the whole
folder instead.

## Known issues

* With frame generation on, labels attached to things in the world, such as
  the nameplate over a companion, can smear slightly when the camera pans.
  The rest of the interface is clean.
* There is no sharpening. DLSS 4 does not sharpen and the mod has no
  sharpening pass of its own yet; a ReShade sharpening effect fills the gap.
* Turning frame generation on or off needs a restart of the game.
* It has been tested on one machine. 1080p and 1440p displays and other
  graphics cards are untested; reports are welcome.

## What this project will not do

* Support a copy of the game that is not a legitimate Steam install.
* Ship NVIDIA's files, or patch them.
* Write to the NVIDIA driver's registry settings or application profiles.
* Present itself to NVIDIA's software as a different application.
* Provide, document or support DLSS 5 neural rendering, or third-party forks
  of OptiScaler.

## Building from source

You need [Rust](https://rustup.rs). The project is built with one pinned
toolchain, named in `rust-toolchain.toml`: Rust 1.98.1 for the GNU Windows
host, which needs no Visual Studio. Install it once, then build:

```powershell
rustup toolchain install 1.98.1-x86_64-pc-windows-gnu --component rustfmt --component clippy
git clone https://github.com/Gargauth/mhworld-dlss
cd mhworld-dlss
cargo test
cargo build --release
```

This produces `target\release\mhw_dlss.dll`, installed as
`<game>\mhw-dlss\mhw-dlss.dll`, and `target\release\launcher.exe`, installed
as `<game>\mhw-dlss\mhw-dlss-launcher.exe`. `deploy.ps1` builds and copies
both. Released binaries are built by
[the release workflow](.github/workflows/release.yml), never on a developer's
machine.

How the mod works is described where it is done: start with the header of
`src/lib.rs`, which maps the source files by what they are for.

## Licence and credits

Copyright (C) 2026 Gargauth.

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License, version 3, as published by the
Free Software Foundation. It is distributed without any warranty. See
[LICENSE](LICENSE), and [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for
the libraries it is built from and the terms of NVIDIA's files.

Thanks to the authors of [ReShade](https://reshade.me),
[RenoDX](https://github.com/clshortfuse/renodx) and
[OptiScaler](https://github.com/optiscaler/OptiScaler), and to NVIDIA for
publishing Streamline.

NVIDIA, DLSS, Reflex and Streamline are trademarks of NVIDIA Corporation.
Monster Hunter: World is a trademark of CAPCOM CO., LTD. This project is not
affiliated with either.
