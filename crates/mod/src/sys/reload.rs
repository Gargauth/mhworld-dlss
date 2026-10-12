//! Hot reload: the settings file is read every 500 ms and, when its bytes
//! changed, parsed and put in force (`runtime::replace`: live groups
//! applied at once, restart groups reported once).

use std::path::Path;
use std::time::Duration;

use mhw_core::config::{parse_bytes, Build, Diagnostic};

use super::runtime;

/// How often the file is read.
const EVERY: Duration = Duration::from_millis(500);

/// Read and parse the settings file; `None` when it cannot be read.
pub(crate) fn read(path: &Path) -> Option<(Vec<u8>, mhw_core::config::Settings, Vec<Diagnostic>)> {
    let bytes = std::fs::read(path).ok()?;
    let (settings, diagnostics) = parse_bytes(&bytes, Build::CURRENT);
    Some((bytes, settings, diagnostics))
}

/// Log each diagnostic once.
pub(crate) fn report(diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        mhw_core::log_once!(
            Warn,
            "config",
            diagnostic.to_string(),
            "{}: {diagnostic}",
            mhw_core::config::FILE_NAME
        );
    }
}

/// Start polling `path`, whose content at start was `bytes`.
pub(crate) fn start(bytes: Vec<u8>) {
    let Some(paths) = runtime::paths() else {
        return;
    };
    let path = paths.settings.clone();
    let spawned = mhw_win::spawn("reload", move || {
        let mut last = bytes;
        loop {
            std::thread::sleep(EVERY);
            let Ok(now) = std::fs::read(&path) else {
                mhw_core::log_once!(
                    Warn,
                    "config",
                    "unreadable",
                    "{} cannot be read; the settings in force stay",
                    path.display()
                );
                continue;
            };
            if now == last {
                continue;
            }
            let (settings, diagnostics) = parse_bytes(&now, Build::CURRENT);
            report(&diagnostics);
            last = now;
            if let Some(replaced) = runtime::replace(settings) {
                if !replaced.changes.is_empty() {
                    log::info!(
                        target: "config",
                        "settings file changed: {:?}",
                        replaced.changes.iter().collect::<Vec<_>>()
                    );
                }
            }
        }
    });
    if let Err(error) = spawned {
        log::error!(target: "config", "hot reload not started: {error}");
    }
}
