//! The authentication token (`docs/DEV-PROTOCOL.md` § Authentication):
//! 32 random bytes as hex in `[Dev] TokenFile`, created on first start and
//! reused afterwards. The value is never logged.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Random bytes in a token.
pub const TOKEN_BYTES: usize = 32;
/// Shortest token accepted from an existing file (a user may write their
/// own; anything shorter is replaced).
pub const MIN_TOKEN_LEN: usize = 16;

/// Lower-case hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The token in a file's text, if usable: trimmed, at least
/// [`MIN_TOKEN_LEN`] visible ASCII characters without spaces.
pub fn parse(text: &str) -> Option<String> {
    let token = text.trim();
    (token.len() >= MIN_TOKEN_LEN && token.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| token.to_owned())
}

/// Constant-time comparison (no early exit on the first difference).
pub fn matches(given: &str, expected: &str) -> bool {
    let (a, b) = (given.as_bytes(), expected.as_bytes());
    let mut diff = a.len() ^ b.len();
    for (i, &x) in b.iter().enumerate() {
        diff |= usize::from(x ^ a.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// Fill `out` from the OS random generator.
#[cfg(windows)]
pub fn os_random(out: &mut [u8]) -> io::Result<()> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    // SAFETY: the buffer is valid for its length; no algorithm handle is
    // used with the system-preferred flag.
    unsafe { BCryptGenRandom(None, out, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .map_err(|e| io::Error::other(format!("BCryptGenRandom: {e}")))
}

/// Fill `out` from the OS random generator.
#[cfg(not(windows))]
pub fn os_random(out: &mut [u8]) -> io::Result<()> {
    use std::io::Read;
    fs::File::open("/dev/urandom")?.read_exact(out)
}

/// The token in `path`, creating the file with a fresh token if it is
/// missing or unusable.
pub fn load_or_create(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            if let Some(token) = parse(&text) {
                return Ok(token);
            }
            log::warn!(target: "dev", "token file {} is unusable; writing a new token", path.display());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut bytes = [0u8; TOKEN_BYTES];
    os_random(&mut bytes)?;
    let token = to_hex(&bytes);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let mut file = fs::File::create(path)?;
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")?;
    log::info!(target: "dev", "created dev token file {}", path.display());
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_gets_a_fresh_token_that_is_then_reused() {
        let dir = std::env::temp_dir().join(format!("mhw-dev-token-{}", std::process::id()));
        let path = dir.join("sub").join("dev.token");
        let _ = fs::remove_dir_all(&dir);
        let first = load_or_create(&path).unwrap();
        assert_eq!(first.len(), 2 * TOKEN_BYTES);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{first}\n"));
        assert_eq!(load_or_create(&path).unwrap(), first);
        // An unusable file is replaced.
        fs::write(&path, "short").unwrap();
        let second = load_or_create(&path).unwrap();
        assert_ne!(second, first);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tokens_parse_and_compare_exactly() {
        assert_eq!(
            parse("  0123456789abcdef\r\n").as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(parse("0123456789 abcdef"), None);
        assert_eq!(parse("tooshort"), None);
        assert!(matches("abc", "abc"));
        assert!(!matches("abd", "abc"));
        assert!(!matches("ab", "abc"));
        assert!(!matches("abcd", "abc"));
        assert!(!matches("", "abc"));
        assert_eq!(to_hex(&[0, 0xab, 0x10]), "00ab10");
    }
}
