# Companion tools: ReShade, RenoDX, OptiScaler, Stracker's Loader

All four are optional. None is part of this project or shipped with it; each
has its own download and its own licence. Each was verified in Monster
Hunter: World on one machine, in the versions named below: ReShade, RenoDX
and OptiScaler together with version 0.2 of this mod, Stracker's Loader with
0.3. Other versions are untested.

`<game>` is the game folder, for example
`C:\Program Files (x86)\Steam\steamapps\common\Monster Hunter World`. The mod
lives in `<game>\mhw-dlss`, and the libraries of ReShade and OptiScaler go
there too.

The one-line installer (see the README) offers to set up ReShade, RenoDX and
OptiScaler for you: it downloads each tool you say yes to from its authors,
checks it, puts it in place and adds its line to the chain file. This page is
the same route by hand.

## The chain file

`<game>\mhw-dlss\mhw-dlss-loader.ini` lists the libraries loaded into the
game, one per line. The launcher writes it on first start with only the mod
in it; the file explains itself in its comments. With both tools it reads:

```text
mhw-dlss.dll
ReShade64.dll  renderer
OptiScaler.dll  renderer
```

A name without a path is looked for in the folder the chain file is in,
which is why the libraries go into `mhw-dlss`. The word `renderer` after a
name means the mod loads that library itself at the moment the game loads
Direct3D 12, before the game creates its device and swapchain. Both tools
need that moment.

Tools like these are normally installed by renaming their DLL to `dxgi.dll`,
`dinput8.dll`, `winmm.dll` or `version.dll`. In this game that does not work:
none of those names is loaded from the game folder early enough. That is why
the chain file exists. Do not run ReShade's installer against the game, and
do not run OptiScaler's `setup_windows.bat`.

## ReShade

Verified with ReShade 6.8.0, the build "with full add-on support", with and
without frame generation.

1. Download ReShade from <https://reshade.me>. Take the build with full
   add-on support if you want RenoDX; the standard build is enough for
   effects only.
2. Do not run the installer against the game. The installer is also an
   archive: open it with 7-Zip, take `ReShade64.dll` out, and put it in
   `<game>\mhw-dlss`.
3. Add the line `ReShade64.dll  renderer` to
   `<game>\mhw-dlss\mhw-dlss-loader.ini`.
4. Start the game through Steam as usual. ReShade's overlay opens with the
   Home key.

**Where ReShade keeps its own files.** Loaded this way, ReShade works from
the folder of the game's executable, not from `mhw-dlss`: it writes
`ReShade.ini` and `ReShade.log` into `<game>`, and looks there for its
presets, its effect shaders and its add-ons unless `ReShade.ini` says
otherwise. Those files are ReShade's and yours; removing the mod leaves them
where they are.

**Settings page.** The mod adds its settings to ReShade's overlay, in its
section of the Add-ons tab. The page edits `<game>\mhw-dlss\mhw-dlss.ini`,
which the mod reads again within a second; editing the file by hand works
the same. If a ReShade build offers no add-on interface, the
`settings page:` line in `mhw-dlss.log` says so.

With frame generation on, ReShade's effects and overlay are applied to every
presented frame, generated ones included.

## RenoDX (HDR)

Verified with the RenoDX add-on for this game, on one HDR display, with frame
generation.

1. Set up ReShade as above, in the build with full add-on support.
2. Get `renodx-mhworld.addon64` from the RenoDX project,
   <https://github.com/clshortfuse/renodx>, and put it where ReShade looks
   for add-ons: in `<game>`, beside the game's executable. Nothing is added
   to the chain file for it.
3. Turn HDR on in Windows (Settings, System, Display).
4. Turn on the game's own HDR output in its display options.

## OptiScaler

Verified with the official OptiScaler 0.9.4 release, alone and together with
ReShade. Third-party forks of OptiScaler are not supported.

With this mod, OptiScaler does not need its usual tricks: the mod already
makes the DLSS calls, and it drives frame generation itself. OptiScaler is
therefore set to stay out of Streamline and frame generation.

1. Download the 0.9.4 release from
   <https://github.com/optiscaler/OptiScaler/releases>, open the archive
   (it is a `.7z`; 7-Zip opens it) and copy two files out of it into
   `<game>\mhw-dlss`: `OptiScaler.dll` and `OptiScaler.ini`. Keep the name
   `OptiScaler.dll`, and do not run `setup_windows.bat`.

   They go in `mhw-dlss`, not in the game folder. In the game folder
   OptiScaler makes NVIDIA's runtime load the game's own old DLSS 1 libraries
   that lie there, and frame generation then fails.

   Those two files are what was verified, and they are enough for
   OptiScaler's overlay and for DLSS through it. Its FSR and XeSS upscalers
   need further libraries from the same archive; copying those beside
   `OptiScaler.dll` has not been tried with this mod.
2. Add the line `OptiScaler.dll  renderer` to
   `<game>\mhw-dlss\mhw-dlss-loader.ini`.
3. In `<game>\mhw-dlss\OptiScaler.ini`, set these seven keys. Each already
   exists in the file under the section shown; replace `<game>` with your
   game folder.

   ```ini
   [FrameGen]
   FGInput=nofg
   FGOutput=nofg

   [Inputs]
   EnableDlssInputs=true

   [Libraries]
   NvngxFeaturePath=<game>\mhw-dlss

   [Menu]
   OverlayMenu=true

   [Spoofing]
   StreamlineSpoofing=false
   Dxgi=false
   ```

`EnableDlssInputs` chooses between two arrangements, both verified:

* `true`: the mod's DLSS calls go through OptiScaler, which sees the mod as
  the game's upscaler and can show its overlay and apply its own upscaler
  options to it.
* `false`: OptiScaler and the mod simply run side by side, and the mod calls
  NVIDIA's DLSS directly.

At start OptiScaler shows a one-off notice, "Late Streamline hook detected".
With this mod that is expected and harmless: Streamline is loaded by the mod,
which drives frame generation, and OptiScaler is not meant to hook it.

The mod sets the texture mip bias for its render scale itself; leave
OptiScaler's own mip bias override off.

## Stracker's Loader

Verified with Stracker's Loader 4.0 and the two plugins that come with it,
QuestLoader and MonsterLoader, with frame generation on.

Install it the way its own instructions say: `dinput8.dll`, `loader.dll`,
`loader-config.json` and the `nativePC` folder go into `<game>`, beside the
game's executable, not into `mhw-dlss`. Nothing is added to the chain file
for it. Start the game through Steam with the launcher as usual.

The game loads `dinput8.dll` only after its renderer exists, so Stracker's
Loader and its plugins start after the mod is in place. That is early enough
for plugins that change the game's data and rules, which is what they do, but
too late for the mod, so the mod cannot be installed as one of its plugins.
A plugin that changes the game's renderer could conflict with the mod; the
two verified ones do not.
