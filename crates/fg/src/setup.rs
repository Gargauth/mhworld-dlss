//! What `slInit` is given, decided before anything is loaded.
//!
//! Pure: where the plugins are, whether the folder is safe to hand to
//! Streamline, which identity route NGX will see, the preference flags,
//! whose signature the interposer must carry, and how Streamline's own log
//! lines are kept from flooding ours.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::abi;

/// Monster Hunter: World's NGX application id, as the game passes it to
/// `NVSDK_NGX_D3D12_Init` (0x5f8a85f). Zero is not an option on either
/// route: Streamline then disables every NGX feature, DLSS-G included.
pub const MHW_APPLICATION_ID: u32 = 100_182_111;

/// The library loaded first; every plugin lives beside it.
pub const INTERPOSER: &str = "sl.interposer.dll";

/// The game's own 2019 DLISP snippet. In Streamline's plugin folder NGX
/// selects and initialises it, it fails `NvAPI_D3D12_CreateCubinComputeShader`
/// and `slSetD3DDevice` ends in `eErrorExceptionHandler` before `sl.dlss_g`
/// starts (measured: same folder without it → DLSS-G loads). The game's
/// capability check does not need this copy; it uses the game folder's.
pub const DLISP_SNIPPET: &str = "nvngx_dlisp.dll";

/// Why frame generation will not be initialised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The mod folder is not an absolute path (Streamline requires
    /// absolute paths).
    NotAbsolute {
        /// The folder given.
        folder: PathBuf,
    },
    /// `sl.interposer.dll` is in neither candidate folder.
    Missing {
        /// The preferred folder that was searched.
        folder: PathBuf,
    },
    /// The plugin folder holds `nvngx_dlisp.dll`.
    DlispSnippet {
        /// Its full path.
        path: PathBuf,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotAbsolute { folder } => write!(
                f,
                "the mod folder {} is not an absolute path, which Streamline requires",
                folder.display()
            ),
            Refusal::Missing { folder } => write!(
                f,
                "{INTERPOSER} is not in {}; frame generation is unavailable (run \
                 fetch-nvidia.cmd in the mod folder to download NVIDIA's files)",
                folder.display()
            ),
            Refusal::DlispSnippet { path } => write!(
                f,
                "{} exists, so Streamline is not initialised and frame generation is off: \
                 NGX initialises that snippet in Streamline's context, it raises an exception \
                 inside slSetD3DDevice and sl.dlss_g never starts. The game's own DLSS check \
                 uses the copy in the game folder, so rename this one to {DLISP_SNIPPET}.disabled",
                path.display()
            ),
        }
    }
}

/// Where the plugins were found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plugins {
    /// The folder handed to `slInit` as plugin and log path.
    pub folder: PathBuf,
    /// `false` when found in the `sl` subfolder: that works, but gives
    /// Streamline's NGX client another application data path than the
    /// game's (2.14 uses the plugin folder for it), worth a warning.
    pub beside_ngx: bool,
}

impl Plugins {
    /// Full path of `sl.interposer.dll` (Streamline requires absolute
    /// paths).
    pub fn interposer(&self) -> PathBuf {
        self.folder.join(INTERPOSER)
    }
}

/// `nvngx_dlisp.dll` beside `host`, a foreign module serving NGX
/// (OptiScaler), when there is one. NGX looks for snippets in the folder of
/// the module that initialises it; in the game folder that is the game's
/// 2019 copy, which raises a breakpoint when NGX initialises it for
/// Streamline on a current GPU: `slSetD3DDevice` ends in
/// `eErrorExceptionHandler` and there is no frame generation (0.2, from a
/// minidump: the breakpoint in the game's snippet under `_nvngx.dll`,
/// `sl.common.dll` and `OptiScaler.dll`). Worth a warning, not a refusal.
pub fn dlisp_beside(host: &Path, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let snippet = host.parent()?.join(DLISP_SNIPPET);
    exists(&snippet).then_some(snippet)
}

/// Find the plugins in `mod_folder` (preferred: beside `nvngx_dlss.dll`)
/// or its `sl` subfolder, and refuse a folder holding the DLISP snippet.
pub fn locate_plugins(
    mod_folder: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Result<Plugins, Refusal> {
    if !mod_folder.is_absolute() {
        return Err(Refusal::NotAbsolute {
            folder: mod_folder.to_path_buf(),
        });
    }
    let nested = mod_folder.join("sl");
    let plugins = if exists(&mod_folder.join(INTERPOSER)) {
        Plugins {
            folder: mod_folder.to_path_buf(),
            beside_ngx: true,
        }
    } else if exists(&nested.join(INTERPOSER)) {
        Plugins {
            folder: nested,
            beside_ngx: false,
        }
    } else {
        return Err(Refusal::Missing {
            folder: mod_folder.to_path_buf(),
        });
    };
    let snippet = plugins.folder.join(DLISP_SNIPPET);
    if exists(&snippet) {
        return Err(Refusal::DlispSnippet { path: snippet });
    }
    Ok(plugins)
}

/// Who we say we are to Streamline and, through it, to NGX.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    /// `NVSDK_NGX_D3D12_Init` with the application id.
    ApplicationId(u32),
    /// `NVSDK_NGX_D3D12_Init_with_ProjectID`. `sl.common` takes this route
    /// only when **both** strings are non-empty, and its requirements query
    /// tests the opposite, so the pair is sent together or not at all.
    /// The application id is still sent (Streamline uses it for its OTA
    /// deny list and logs it).
    Project {
        /// Application id, still sent.
        application_id: u32,
        /// `sl::EngineType` ([`abi::engine`]).
        engine: u32,
        /// `engineVersion`.
        engine_version: String,
        /// `projectId`, a GUID.
        project_id: String,
    },
}

impl Default for Identity {
    fn default() -> Identity {
        Identity::ApplicationId(MHW_APPLICATION_ID)
    }
}

/// The identity fields of `sl::Preferences`, resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityFields {
    /// `applicationId`, never zero.
    pub application_id: u32,
    /// `engine`.
    pub engine: u32,
    /// `engineVersion` with its NUL, `None` for a null pointer.
    pub engine_version: Option<Vec<u8>>,
    /// `projectId` with its NUL, `None` for a null pointer.
    pub project_id: Option<Vec<u8>>,
    /// Which NGX entry point Streamline will call.
    pub route: &'static str,
    /// The project route was asked for without both strings.
    pub fell_back: bool,
}

fn c_string(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.is_empty() || text.contains('\0') {
        return None;
    }
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    Some(bytes)
}

impl Identity {
    /// Resolve into the fields `slInit` takes. An application id of zero is
    /// replaced with [`MHW_APPLICATION_ID`].
    pub fn fields(&self) -> IdentityFields {
        let id = |value: u32| {
            if value == 0 {
                MHW_APPLICATION_ID
            } else {
                value
            }
        };
        match self {
            Identity::ApplicationId(value) => IdentityFields {
                application_id: id(*value),
                engine: abi::engine::CUSTOM,
                engine_version: None,
                project_id: None,
                route: "NVSDK_NGX_D3D12_Init",
                fell_back: false,
            },
            Identity::Project {
                application_id,
                engine,
                engine_version,
                project_id,
            } => match (c_string(engine_version), c_string(project_id)) {
                (Some(version), Some(project)) => IdentityFields {
                    application_id: id(*application_id),
                    engine: *engine,
                    engine_version: Some(version),
                    project_id: Some(project),
                    route: "NVSDK_NGX_D3D12_Init_with_ProjectID",
                    fell_back: false,
                },
                _ => IdentityFields {
                    fell_back: true,
                    ..Identity::ApplicationId(*application_id).fields()
                },
            },
        }
    }
}

/// `sl::Preferences::flags`.
///
/// Manual hooking, because Streamline cannot replace `dxgi.dll` in this
/// game's load order; the DXGI factory proxy, which exists for injected
/// libraries by name and keeps Streamline's interposition an object of its
/// own; frame-based tagging, without which `slSetTagForFrame` is not
/// switched on. `allow_ota` adds Streamline's own default of fetching and
/// loading updated plugins and models over the air.
pub fn preference_flags(allow_ota: bool) -> u64 {
    use abi::preference::*;
    let base = DISABLE_CL_STATE_TRACKING
        | USE_MANUAL_HOOKING
        | USE_DXGI_FACTORY_PROXY
        | USE_FRAME_BASED_RESOURCE_TAGGING;
    if allow_ota {
        base | ALLOW_OTA | LOAD_DOWNLOADED_PLUGINS
    } else {
        base
    }
}

/// How much Streamline logs (`sl::LogLevel`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SlLogLevel {
    /// Nothing.
    Off,
    /// Streamline's default.
    #[default]
    Default,
    /// Everything, including NGX's forwarded lines.
    Verbose,
}

impl SlLogLevel {
    /// The ABI value.
    pub const fn to_abi(self) -> u32 {
        match self {
            SlLogLevel::Off => abi::log_level::OFF,
            SlLogLevel::Default => abi::log_level::DEFAULT,
            SlLogLevel::Verbose => abi::log_level::VERBOSE,
        }
    }
}

/// Organisation and common name required of the interposer's signer, as
/// the fetch script required them of every NVIDIA file.
pub const SIGNER: &str = "NVIDIA Corporation";

/// Whether a signer certificate's subject names NVIDIA in both the common
/// name and the organisation, as whole values.
pub fn signer_is_nvidia(common_name: &str, organisation: &str) -> bool {
    common_name.trim() == SIGNER && organisation.trim() == SIGNER
}

/// What a `WinVerifyTrust` status means, for the refusal message.
pub fn trust_status_name(status: i32) -> &'static str {
    match status as u32 {
        0 => "valid",
        0x800B_0100 => "not signed (TRUST_E_NOSIGNATURE)",
        0x8009_6010 => "modified after signing (TRUST_E_BAD_DIGEST)",
        0x800B_0109 => "signed with an untrusted root (CERT_E_UNTRUSTEDROOT)",
        0x800B_0101 => "signed with an expired certificate (CERT_E_EXPIRED)",
        0x800B_010A => "certificate chain incomplete (CERT_E_CHAINING)",
        0x800B_0111 => "signer explicitly distrusted (TRUST_E_EXPLICIT_DISTRUST)",
        0x800B_0004 => "subject not trusted (TRUST_E_SUBJECT_NOT_TRUSTED)",
        0x800B_0001 => "no trust provider for the file type (TRUST_E_PROVIDER_UNKNOWN)",
        0x8009_2026 => "refused by local security policy (CRYPT_E_SECURITY_SETTINGS)",
        _ => "rejected",
    }
}

/// A line's template: runs of digits (frame numbers, addresses, counts)
/// replaced by `#`, so a message repeated per frame is one template.
pub fn template(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_digits = false;
    for c in line.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            out.push(c);
            in_digits = false;
        }
    }
    out
}

/// Keeps Streamline's repeated lines from flooding the log.
///
/// The first [`RepeatFilter::ALLOWANCE`] lines pass unconditionally: that
/// covers start-up, where features are decided. After that each template
/// is written on its 1st, 10th, 100th … occurrence, with the count.
/// Templates beyond [`RepeatFilter::TEMPLATES`] are dropped and counted.
#[derive(Debug, Default)]
pub struct RepeatFilter {
    passed: u64,
    seen: HashMap<String, u64>,
    dropped: u64,
}

impl RepeatFilter {
    /// Lines written before filtering starts.
    pub const ALLOWANCE: u64 = 4000;
    /// Distinct templates remembered.
    pub const TEMPLATES: usize = 512;

    /// An empty filter.
    pub fn new() -> RepeatFilter {
        RepeatFilter::default()
    }

    /// `Some(count)` when `line` should be written (`count` is how many
    /// times its template has been seen, 1 during the allowance).
    pub fn admit(&mut self, line: &str) -> Option<u64> {
        if self.passed < Self::ALLOWANCE {
            self.passed += 1;
            return Some(1);
        }
        let key = template(line);
        let full = self.seen.len() >= Self::TEMPLATES;
        let count = match self.seen.get_mut(&key) {
            Some(count) => {
                *count += 1;
                *count
            }
            None if !full => {
                self.seen.insert(key, 1);
                1
            }
            None => {
                self.dropped += 1;
                return None;
            }
        };
        let mut power = 1;
        while power < count {
            power = power.saturating_mul(10);
        }
        (power == count).then_some(count)
    }

    /// Lines dropped because the template table was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `p` (written with a leading `/`) as an absolute path on the host:
    /// Windows needs a drive for `Path::is_absolute`.
    fn abs(p: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{p}"))
        } else {
            PathBuf::from(p)
        }
    }

    fn files(present: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |path: &Path| present.iter().any(|p| abs(p) == path)
    }

    #[test]
    fn plugins_beside_ngx_are_preferred() {
        let found = locate_plugins(
            &abs("/g/mhw-dlss"),
            files(&[
                "/g/mhw-dlss/sl.interposer.dll",
                "/g/mhw-dlss/sl/sl.interposer.dll",
            ]),
        )
        .unwrap();
        assert_eq!(found.folder, abs("/g/mhw-dlss"));
        assert!(found.beside_ngx);
        assert_eq!(found.interposer(), abs("/g/mhw-dlss/sl.interposer.dll"));
    }

    #[test]
    fn a_dlisp_snippet_beside_a_foreign_ngx_host_is_found() {
        let host = abs("/g/OptiScaler.dll");
        assert_eq!(
            dlisp_beside(&host, files(&["/g/nvngx_dlisp.dll"])),
            Some(abs("/g/nvngx_dlisp.dll"))
        );
        assert_eq!(dlisp_beside(&host, files(&["/g/m/nvngx_dlisp.dll"])), None);
        assert_eq!(dlisp_beside(Path::new(""), |_| true), None);
    }

    #[test]
    fn the_sl_subfolder_is_accepted_and_flagged() {
        let found = locate_plugins(
            &abs("/g/mhw-dlss"),
            files(&["/g/mhw-dlss/sl/sl.interposer.dll"]),
        )
        .unwrap();
        assert_eq!(found.folder, abs("/g/mhw-dlss/sl"));
        assert!(!found.beside_ngx);
    }

    #[test]
    fn missing_plugins_and_the_dlisp_snippet_are_refused() {
        assert_eq!(
            locate_plugins(Path::new("relative/m"), |_| true),
            Err(Refusal::NotAbsolute {
                folder: "relative/m".into()
            })
        );
        assert_eq!(
            locate_plugins(&abs("/g/m"), files(&[])),
            Err(Refusal::Missing {
                folder: abs("/g/m")
            })
        );
        let refused = locate_plugins(
            &abs("/g/m"),
            files(&["/g/m/sl.interposer.dll", "/g/m/nvngx_dlisp.dll"]),
        );
        assert_eq!(
            refused,
            Err(Refusal::DlispSnippet {
                path: abs("/g/m/nvngx_dlisp.dll")
            })
        );
        let message = refused.unwrap_err().to_string();
        assert!(message.contains("rename"), "{message}");
        // Only the folder Streamline gets matters.
        assert!(locate_plugins(
            &abs("/g/m"),
            files(&["/g/m/sl/sl.interposer.dll", "/g/m/nvngx_dlisp.dll"]),
        )
        .is_ok());
    }

    #[test]
    fn identity_routes() {
        let app = Identity::default().fields();
        assert_eq!(app.application_id, MHW_APPLICATION_ID);
        assert_eq!(app.route, "NVSDK_NGX_D3D12_Init");
        assert_eq!((app.engine_version, app.project_id), (None, None));
        assert_eq!(
            Identity::ApplicationId(0).fields().application_id,
            MHW_APPLICATION_ID
        );

        let project = Identity::Project {
            application_id: 7,
            engine: abi::engine::CUSTOM,
            engine_version: "1.0".into(),
            project_id: "a0f57b54-1daf-4934-90ae-c4035c19df04".into(),
        }
        .fields();
        assert_eq!(project.route, "NVSDK_NGX_D3D12_Init_with_ProjectID");
        assert_eq!(project.application_id, 7);
        assert_eq!(project.engine_version.as_deref(), Some(&b"1.0\0"[..]));
        assert!(!project.fell_back);

        let half = Identity::Project {
            application_id: 7,
            engine: abi::engine::UNREAL,
            engine_version: "1.0".into(),
            project_id: "  ".into(),
        }
        .fields();
        assert!(half.fell_back);
        assert_eq!(half.route, "NVSDK_NGX_D3D12_Init");
        assert_eq!(half.engine, abi::engine::CUSTOM);
        assert_eq!(half.engine_version, None);
    }

    #[test]
    fn ota_is_a_choice_and_the_rest_is_fixed() {
        use abi::preference::*;
        let without = preference_flags(false);
        assert_eq!(without & (ALLOW_OTA | LOAD_DOWNLOADED_PLUGINS), 0);
        for flag in [
            USE_MANUAL_HOOKING,
            USE_DXGI_FACTORY_PROXY,
            USE_FRAME_BASED_RESOURCE_TAGGING,
            DISABLE_CL_STATE_TRACKING,
        ] {
            assert_ne!(without & flag, 0);
        }
        assert_eq!(
            preference_flags(true),
            without | ALLOW_OTA | LOAD_DOWNLOADED_PLUGINS
        );
    }

    #[test]
    fn trust_failures_are_named() {
        assert_eq!(trust_status_name(0), "valid");
        assert!(trust_status_name(0x800B_0100_u32 as i32).contains("not signed"));
        assert_eq!(trust_status_name(-1), "rejected");
    }

    #[test]
    fn only_nvidia_signs_the_interposer() {
        assert!(signer_is_nvidia("NVIDIA Corporation", "NVIDIA Corporation"));
        assert!(!signer_is_nvidia("NVIDIA Corporation", "Someone Else"));
        assert!(!signer_is_nvidia(
            "NVIDIA Corporation Ltd",
            "NVIDIA Corporation"
        ));
        assert!(!signer_is_nvidia("", ""));
    }

    #[test]
    fn repeated_lines_thin_out_after_startup() {
        assert_eq!(
            template("Unable to find constants for frame 3545 - id 1"),
            "Unable to find constants for frame # - id #"
        );
        let mut filter = RepeatFilter::new();
        for i in 0..RepeatFilter::ALLOWANCE {
            assert_eq!(filter.admit(&format!("startup {i}")), Some(1));
        }
        let written: Vec<u64> = (0..1000)
            .filter_map(|i| filter.admit(&format!("frame {i} missing")))
            .collect();
        assert_eq!(written, [1, 10, 100, 1000]);
        // One template is already known; fill the table up to its limit.
        for i in 0..RepeatFilter::TEMPLATES - 1 {
            filter.admit(&"x".repeat(i + 1));
        }
        assert_eq!(filter.admit("brand new"), None);
        assert_eq!(filter.dropped(), 1);
    }
}
