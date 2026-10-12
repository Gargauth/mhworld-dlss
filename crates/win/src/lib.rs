//! Windows primitives for mhw-dlss; see `docs/ARCHITECTURE.md`, section
//! `mhw-win`.
//!
//! Pure logic (PE parsing, the x64 prologue decoder, fault bookkeeping) lives
//! in `cfg`-free modules tested on any host; everything that touches the
//! process is `#[cfg(windows)]`:
//!
//! * [`Module`]: a loaded image, its sections, timestamp and checked reads;
//!   [`loaded_modules`] in load order.
//! * [`mem`]: checked reads, writes with protection change and instruction
//!   cache flush, atomic pointer writes.
//! * [`code`]: [`ThreadSuspender`] and all-or-nothing code patches.
//! * [`Detour`]: `jmp [rip+0]` detours with trampolines.
//! * [`patch_iat`] (by name or ordinal, regular and delay imports) and
//!   [`patch_vtable`].
//! * [`install_crash_logger`]: vectored fault logging, target `crash`.
//! * [`Event`] and thread helpers.

pub mod fault;
pub mod pe;
pub mod x64;

#[cfg(windows)]
pub mod code;
#[cfg(windows)]
mod crash;
#[cfg(windows)]
mod detour;
#[cfg(windows)]
mod iat;
#[cfg(windows)]
pub mod mem;
#[cfg(windows)]
mod module;
#[cfg(windows)]
mod sync;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod vtable;

pub use pe::{ImportRef, ImportTable};

#[cfg(windows)]
pub use code::{CodePatch, PatchError, ThreadSuspender};
#[cfg(windows)]
pub use crash::{install_crash_logger, remove_crash_logger, TARGET as CRASH_TARGET};
#[cfg(windows)]
pub use detour::{install as install_detours, Detour, DetourError};
#[cfg(windows)]
pub use iat::{patch_iat, IatError, IatPatch};
#[cfg(windows)]
pub use module::{loaded_modules, module_base_of, module_path, LoadedModule, Module, ModuleError};
#[cfg(windows)]
pub use sync::{current_thread_id, spawn, Event, ForeignLock, Wait};
#[cfg(windows)]
pub use vtable::{patch_vtable, vtable_of, write_vtable, VtableError};
