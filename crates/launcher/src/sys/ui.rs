//! Telling the user something without a console: message boxes, and a
//! console only when asked for with `--verbose`.

use std::fs::{File, OpenOptions};

use mhw_dlss_launcher::TARGET;
use windows::core::PCWSTR;
use windows::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDRETRY, MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_RETRYCANCEL, MB_SETFOREGROUND,
    MB_TOPMOST, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE,
};

const TITLE: &str = "mhw-dlss launcher";

/// A NUL-terminated UTF-16 copy of `text`.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// How serious a message is.
#[derive(Debug, Clone, Copy)]
pub enum Severity {
    /// The launch did not happen.
    Error,
    /// The launch happened without something.
    Warning,
}

/// Log `text` and show it in a message box.
pub fn tell(text: &str, severity: Severity) {
    let icon = match severity {
        Severity::Error => {
            log::error!(target: TARGET, "{text}");
            MB_ICONERROR
        }
        Severity::Warning => {
            log::warn!(target: TARGET, "{text}");
            MB_ICONWARNING
        }
    };
    show(text, MB_OK | icon);
}

/// Log `text`, ask Retry or Cancel, and return whether Retry was pressed.
pub fn retry(text: &str) -> bool {
    log::warn!(target: TARGET, "{text}");
    show(text, MB_RETRYCANCEL | MB_ICONWARNING) == IDRETRY
}

fn show(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let body = wide(text);
    let title = wide(TITLE);
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_SETFOREGROUND | MB_TOPMOST | style,
        )
    }
}

/// The console this program was started from, or a new one.
///
/// Opened by name: a GUI-subsystem process has no standard handles, and
/// attaching to a console does not set them.
pub fn open_console() -> Option<File> {
    // SAFETY: no preconditions; each fails harmlessly when not possible.
    let attached =
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS).is_ok() || AllocConsole().is_ok() };
    if !attached {
        return None;
    }
    OpenOptions::new().write(true).open("CONOUT$").ok()
}
