//! Dev builds: write the bytecode of the volumetric and fog shaders the game
//! creates to a folder (`[Dev] DumpShaders`), for offline disassembly, and
//! build compute pipelines from replacement shaders found there.
//!
//! A shader qualifies for the dump when one of its bindings is named like
//! the game's volume renderer (`CBVR*`) or fog (`*Fog*`, `*Volume*`). Each
//! bytecode is written once, as `<fnv1a>.dxbc`, and listed in `index.txt`
//! with its bindings and constant buffer layouts.
//!
//! A compute shader whose hash has `override/<fnv1a>.hlsl` in the folder is
//! replaced by that file compiled (`main`, `cs_5_0`) when the game creates
//! its pipeline. The replacement must bind within the game's root
//! signature; a compile error keeps the game's shader.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;

use crate::dxbc::{self, Reflection};

struct Dump {
    folder: Option<PathBuf>,
    written: HashSet<u64>,
    /// Compiled overrides by the game shader's hash; `None`: none or failed.
    overrides: HashMap<u64, Option<Arc<[u8]>>>,
}

static DUMP: LazyLock<Mutex<Dump>> = LazyLock::new(|| {
    Mutex::new(Dump {
        folder: None,
        written: HashSet::new(),
        overrides: HashMap::new(),
    })
});

/// Dump qualifying shaders to `folder` (created when needed) and take
/// overrides from `folder/override`, or stop with `None`. Call before the
/// game creates its pipelines.
pub fn configure_shader_dump(folder: Option<PathBuf>) {
    DUMP.lock().folder = folder;
}

/// The replacement for the compute shader `code`, if the folder has one.
pub(crate) fn replacement(code: &[u8]) -> Option<Arc<[u8]>> {
    let mut dump = DUMP.lock();
    let folder = dump.folder.clone()?;
    let hash = dxbc::fnv1a(code);
    if let Some(known) = dump.overrides.get(&hash) {
        return known.clone();
    }
    let path = folder.join("override").join(format!("{hash:016x}.hlsl"));
    let compiled = fs::read_to_string(&path).ok().and_then(|source| {
        match super::gpu::compile(&source, b"mhw_override\0", b"main\0", b"cs_5_0\0") {
            Ok(code) => {
                log::info!(target: "render", "shader {hash:016x} replaced by {}", path.display());
                Some(Arc::from(code))
            }
            Err(error) => {
                log::warn!(target: "render", "override {} does not compile, the game's shader stays: {error}", path.display());
                None
            }
        }
    });
    dump.overrides.insert(hash, compiled.clone());
    compiled
}

fn qualifies(reflection: &Reflection) -> bool {
    reflection.bindings.iter().any(|binding| {
        let name = binding.name.as_str();
        name.starts_with("CBVR") || name.contains("Fog") || name.contains("Volume")
    })
}

/// Offer a created pipeline's shader.
pub(crate) fn offer(code: &[u8], reflection: &Reflection) {
    let mut dump = DUMP.lock();
    let Some(folder) = dump.folder.clone() else {
        return;
    };
    if !qualifies(reflection) {
        return;
    }
    let hash = dxbc::fnv1a(code);
    if !dump.written.insert(hash) {
        return;
    }
    if let Err(error) = write(&folder, hash, code, reflection) {
        log::warn!(target: "render", "shader dump to {} failed: {error}", folder.display());
        dump.folder = None;
    }
}

fn write(folder: &PathBuf, hash: u64, code: &[u8], reflection: &Reflection) -> std::io::Result<()> {
    fs::create_dir_all(folder)?;
    fs::write(folder.join(format!("{hash:016x}.dxbc")), code)?;
    let mut line = format!(
        "{hash:016x} {:?} sm{}.{} {}",
        reflection.stage,
        reflection.model.0,
        reflection.model.1,
        reflection.summary()
    );
    for buffer in &reflection.buffers {
        let _ = write!(line, " | {}[{}]:", buffer.name, buffer.size);
        for variable in &buffer.variables {
            let _ = write!(
                line,
                " {}@{}+{}",
                variable.name, variable.offset, variable.size
            );
        }
    }
    line.push('\n');
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join("index.txt"))?
        .write_all(line.as_bytes())
}
