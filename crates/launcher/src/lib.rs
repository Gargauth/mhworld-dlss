//! The launcher: starts Monster Hunter: World suspended, injects the
//! libraries listed in the chain file, and resumes it.
//!
//! # Why a launcher
//!
//! The mod has to be in the process before the game creates its D3D12
//! device, DXGI factory, swapchain, upload arena and render target views,
//! and before NGX resolves its entry points. No library the game loads from
//! its own folder is early enough *and* safe to replace without one
//! (measured load order: `docs/re/ReLoadInitDx11.md` §2.7):
//!
//! | library            | how the game gets it |
//! | ------------------ | -------------------- |
//! | `d3d12.dll`, `dxgi.dll`, `nvapi64.dll` | `LOAD_LIBRARY_SEARCH_SYSTEM32`: never from the game folder |
//! | `winmm.dll`        | from System32 by Steam's overlay, at process start |
//! | `version.dll`, `iphlpapi.dll` | from System32 by `steamclient64.dll`, before `WinMain` |
//! | `dinput8.dll`, `xinput9_1_0.dll`, `xaudio2_9.dll` | from the game folder, but after the swapchain and NGX exist |
//! | `steam_api64.dll`, `amd_ags_x64.dll` | from the game folder before the device, but they are the game's own files: a proxy replaces them and has to forward to the original |
//! | `atiadlxx.dll`     | tried by `agsInit` before the device; on AMD it is the real ADL in System32, which a copy in the game folder would shadow |
//!
//! Creating the process suspended and injecting before its first
//! instruction sidesteps the loader entirely: the remote `LoadLibraryW`
//! thread is the first thread to run, so the process initialises on it and
//! the mod's `DllMain` has run before the game's main thread is resumed.
//!
//! # Usage
//!
//! As the Steam launch option, so that the game still starts through Steam
//! and inherits its environment:
//!
//! ```text
//! "<game>\mhw-dlss\mhw-dlss-launcher.exe" %command%
//! "<game>\mhw-dlss\mhw-dlss-launcher.exe" --verbose %command%
//! ```
//!
//! The pure parts live here and are tested on any host; the Windows glue is
//! in the binary (`src/main.rs`, `src/sys`).

pub mod chain;
pub mod cmdline;
pub mod handoff;
pub mod layout;
pub mod logging;
pub mod version;

/// The log target of everything the launcher logs.
pub const TARGET: &str = "launcher";
