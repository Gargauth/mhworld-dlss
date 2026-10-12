//! Authenticode check of `sl.interposer.dll` before it is loaded.
//!
//! The same verdict the fetch script required of every NVIDIA file at
//! install time (`Get-AuthenticodeSignature` status `Valid`, signer
//! `CN=NVIDIA Corporation, O=NVIDIA Corporation`), repeated at run time
//! because the folder is writable and the DLSS-G guide asks production
//! integrations to validate the interposer's certificate. Streamline itself
//! then checks the signatures of the plugins it loads.
//!
//! Offline: revocation is not checked and only cached URLs are retrieved,
//! so a game start never waits on the network.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{BOOL, HANDLE, HWND};
use windows::Win32::Security::Cryptography::{
    szOID_COMMON_NAME, szOID_ORGANIZATION_NAME, CertGetNameStringW, CERT_CONTEXT,
    CERT_NAME_ATTR_TYPE,
};
use windows::Win32::Security::WinTrust::{
    WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData,
    WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
    WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOKE_NONE,
    WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};

use crate::setup::{signer_is_nvidia, trust_status_name, SIGNER};

/// Verify `path`: a valid Authenticode signature whose signer certificate
/// names NVIDIA Corporation as common name and organisation. `Ok` carries
/// the signer's common name; `Err` says why the file is refused.
pub(crate) fn verify(path: &Path) -> Result<String, String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(wide.as_ptr()),
        hFile: HANDLE::default(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    };
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: `data` and `file` are fully initialised and outlive the call;
    // no UI is shown (`WTD_UI_NONE`, null window).
    let status = unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
        )
    };
    let outcome = if status != 0 {
        Err(format!(
            "{} is refused: its signature is {} ({:#010x})",
            path.display(),
            trust_status_name(status),
            status as u32
        ))
    } else {
        // SAFETY: a successful verification leaves its provider state open
        // until the close below.
        match unsafe { signer_names(data.hWVTStateData) } {
            Some((common, organisation)) if signer_is_nvidia(&common, &organisation) => Ok(common),
            Some((common, organisation)) => Err(format!(
                "{} is refused: signed by CN={common}, O={organisation}, not {SIGNER}",
                path.display()
            )),
            None => Err(format!(
                "{} is refused: its signer certificate could not be read",
                path.display()
            )),
        }
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: closes the state the verification opened; same structures.
    unsafe {
        WinVerifyTrust(
            HWND::default(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
        );
    }
    outcome
}

/// Common name and organisation of the primary signer's certificate.
///
/// # Safety
/// `state` must be the open state data of a successful verification.
unsafe fn signer_names(state: HANDLE) -> Option<(String, String)> {
    // SAFETY: as the caller guarantees; every pointer is checked before use.
    unsafe {
        let provider = WTHelperProvDataFromStateData(state);
        if provider.is_null() {
            return None;
        }
        let signer = WTHelperGetProvSignerFromChain(provider, 0, BOOL(0), 0);
        if signer.is_null() {
            return None;
        }
        let certificate = WTHelperGetProvCertFromChain(signer, 0);
        if certificate.is_null() || (*certificate).pCert.is_null() {
            return None;
        }
        let context = (*certificate).pCert;
        Some((
            attribute(context, szOID_COMMON_NAME.0.cast())?,
            attribute(context, szOID_ORGANIZATION_NAME.0.cast())?,
        ))
    }
}

/// One subject attribute of `context`, by OID.
///
/// # Safety
/// `context` must be a live certificate context and `oid` a NUL-terminated
/// OID string.
unsafe fn attribute(context: *const CERT_CONTEXT, oid: *const c_void) -> Option<String> {
    let mut buffer = [0u16; 256];
    // SAFETY: as the caller guarantees; the buffer length is passed along.
    let written = unsafe {
        CertGetNameStringW(
            context,
            CERT_NAME_ATTR_TYPE,
            0,
            Some(oid),
            Some(&mut buffer),
        )
    } as usize;
    // `written` counts the terminating NUL; 1 means an empty string.
    let text = buffer.get(..written.saturating_sub(1))?;
    (!text.is_empty()).then(|| String::from_utf16_lossy(text))
}
