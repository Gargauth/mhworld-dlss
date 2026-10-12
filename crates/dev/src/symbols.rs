//! The address book `symbols` returns and memory expressions resolve
//! against, and the write whitelist: `mem.write` may only touch the resolved
//! engine structs (`RenderConfig`, `Camera`, `RenderSettings`, `TaaFilter`,
//! `Device`) unless the request says `force: true`.

use std::collections::BTreeMap;

use mhw_engine::layout::{camera, device, render_config, taa};
use mhw_engine::sites::{field, FIELDS};
use serde_json::{json, Map, Value};

use crate::expr::{hex, ValueType};
use crate::proto::{RpcError, RpcResult};

/// Short names for the engine structs (the engine's own prefixes and the
/// client's habits).
pub const ALIASES: &[(&str, &str)] = &[
    ("cfg", "RenderConfig"),
    ("cam", "Camera"),
    ("ctx", "Camera"),
    ("rs", "RenderSettings"),
    ("settings", "RenderSettings"),
    ("taa", "TaaFilter"),
    ("ss_node", "TaaFilter"),
    ("dev", "Device"),
];

/// Size of the render settings object the engine views check
/// (`mhw_engine::views`, private there: it covers the furthest field used).
pub const RENDER_SETTINGS_SIZE: u64 = 0xea00;

/// A memory range belonging to one engine struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    /// Struct name.
    pub name: String,
    /// First byte.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
}

impl Region {
    /// Whether `[address, address + len)` lies inside.
    pub fn contains(&self, address: u64, len: u64) -> bool {
        address >= self.start
            && address
                .checked_add(len)
                .is_some_and(|end| end <= self.start.saturating_add(self.len))
    }
}

/// One documented field of an engine struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// Name (reflection name where the RE recovered one).
    pub name: String,
    /// Offset in this build.
    pub offset: u64,
    /// How to read it, when known.
    pub ty: Option<ValueType>,
}

/// The resolved address book.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Symbols {
    /// Name → address: the engine structs, `site.<NAME>` and
    /// `global.<NAME>`.
    pub names: BTreeMap<String, u64>,
    /// The engine structs now resolved (the write whitelist).
    pub regions: Vec<Region>,
    /// Struct name → its fields.
    pub fields: BTreeMap<String, Vec<Field>>,
}

impl Symbols {
    /// Add a resolved engine struct: a name and a writable region.
    pub fn add_struct(&mut self, name: &str, start: u64, len: u64) {
        self.names.insert(name.to_owned(), start);
        self.regions.push(Region {
            name: name.to_owned(),
            start,
            len,
        });
    }

    /// The address of `name` or of the struct an alias stands for.
    pub fn lookup(&self, name: &str) -> Option<u64> {
        let name = ALIASES
            .iter()
            .find(|(alias, _)| *alias == name)
            .map_or(name, |(_, target)| target);
        self.names.get(name).copied()
    }

    /// The struct `[address, address + len)` lies in entirely.
    pub fn region_of(&self, address: u64, len: u64) -> Option<&Region> {
        self.regions.iter().find(|r| r.contains(address, len))
    }

    /// Whether a write of `len` bytes at `address` is allowed.
    pub fn check_write(&self, address: u64, len: u64, force: bool) -> RpcResult<()> {
        if force || self.region_of(address, len).is_some() {
            return Ok(());
        }
        Err(RpcError::refused(format!(
            "{len} bytes at {address:#x} are not inside one of the engine structs \
             (RenderConfig, Camera, RenderSettings, TaaFilter, Device); force:true to override"
        )))
    }

    /// The `symbols` result.
    pub fn to_json(&self) -> Value {
        let names: Map<String, Value> = self
            .names
            .iter()
            .map(|(n, a)| (n.clone(), hex(*a)))
            .collect();
        let aliases: Map<String, Value> = ALIASES
            .iter()
            .map(|(a, t)| ((*a).to_owned(), json!(t)))
            .collect();
        let fields: Map<String, Value> = self
            .fields
            .iter()
            .map(|(s, fs)| {
                let table: Map<String, Value> = fs
                    .iter()
                    .map(|f| {
                        (
                            f.name.clone(),
                            json!({"offset": hex(f.offset), "as": f.ty.map(ValueType::name)}),
                        )
                    })
                    .collect();
                (s.clone(), Value::Object(table))
            })
            .collect();
        let regions: Vec<Value> = self
            .regions
            .iter()
            .map(|r| json!({"name": r.name, "addr": hex(r.start), "len": r.len, "writable": true}))
            .collect();
        json!({"symbols": names, "aliases": aliases, "fields": fields, "regions": regions})
    }
}

/// Types and names of the anchored fields, by `sites::field` index (from
/// how `mhw_engine::views` reads them).
const ANCHORED: &[(usize, &str, ValueType)] = &[
    (field::CFG_78, "device", ValueType::U64),
    (field::CFG_198, "mScreenSize", ValueType::U32),
    (field::CFG_1F448, "mBackBufferSize", ValueType::U32),
    (field::CFG_A5, "mUseDLSS", ValueType::U8),
    (field::CFG_A6, "mUseDLISP", ValueType::U8),
    (field::CFG_A8, "mDLPerfQuality", ValueType::U32),
    (field::CFG_B4, "optimal_flags", ValueType::U32),
    (field::CFG_BC, "mUseCAS", ValueType::U8),
    (field::CFG_C0, "mCASUserSharpness", ValueType::F32),
    (field::CFG_1F0, "own_scale", ValueType::F32),
    (field::CFG_1F8, "ContentScalePF", ValueType::F32),
    (field::CFG_1FC, "scale_changed", ValueType::U8),
    (field::CFG_200, "live_ratio", ValueType::F32),
    (field::CFG_204, "built_ratio", ValueType::F32),
    (field::CFG_208, "product", ValueType::F32),
    (field::CFG_1F450, "mpPrimaryScene", ValueType::U64),
    (field::CFG_7B3B0, "dynamic_resolution", ValueType::U8),
    (field::CFG_7B3B4, "mDynamicResolutionMode", ValueType::U32),
    (field::CAM_38, "jitter_x", ValueType::F32),
    (field::CAM_3C, "jitter_y", ValueType::F32),
    (field::CAM_D50, "rect", ValueType::U32),
    (field::RS_1A8, "sampler_bank", ValueType::U32),
    (field::RS_260, "mTextureTargetMode", ValueType::U32),
    (field::RS_E9E4, "anti_aliasing", ValueType::U16),
    (field::RS_E9E5, "fxaa", ValueType::U8),
    (
        field::RS_E964,
        "mCheckerboardAlphaUnrollNear",
        ValueType::F32,
    ),
    (
        field::RS_E968,
        "mCheckerboardAlphaUnrollFar",
        ValueType::F32,
    ),
    (field::RS_B43F, "mSSLRDitherResolve", ValueType::U8),
    (field::RS_B440, "mSSLRGBufferJitter", ValueType::U8),
    (field::RS_B500, "mContactShadowEnableNoise", ValueType::U8),
    (
        field::RS_B501,
        "mFacialContactShadowEnableNoise",
        ValueType::U8,
    ),
    (field::TAA_150, "colour", ValueType::U64),
    (field::TAA_168, "history", ValueType::U64),
    (field::TAA_1A8, "dense_velocity", ValueType::U64),
];

/// Unanchored fields (`mhw_engine::layout`).
const UNANCHORED: &[(&str, &str, usize, ValueType)] = &[
    (
        "RenderConfig",
        "render_frame",
        render_config::RENDER_FRAME,
        ValueType::U32,
    ),
    (
        "Camera",
        "jitter_stamp",
        camera::JITTER_STAMP,
        ValueType::U32,
    ),
    ("Camera", "blend", camera::BLEND, ValueType::F32),
    ("Camera", "viewports", camera::VIEWPORTS, ValueType::U64),
    ("TaaFilter", "mBlendRate", taa::BLEND_RATE, ValueType::F32),
    (
        "TaaFilter",
        "mJitterEnable",
        taa::JITTER_ENABLE,
        ValueType::U8,
    ),
    (
        "TaaFilter",
        "history_invalid",
        taa::HISTORY_INVALID,
        ValueType::U8,
    ),
    ("TaaFilter", "mSharpen", taa::SHARPEN, ValueType::U8),
    (
        "TaaFilter",
        "mSharpenAmount",
        taa::SHARPEN_AMOUNT,
        ValueType::F32,
    ),
    ("Device", "presented", device::PRESENTED, ValueType::U8),
    ("Device", "wait_latch", device::WAIT_LATCH, ValueType::U8),
    ("Device", "waitable", device::WAITABLE, ValueType::U64),
    (
        "Device",
        "dlss_context",
        device::DLSS_CONTEXT,
        ValueType::U64,
    ),
    ("Device", "dlss_ready", device::DLSS_READY, ValueType::U8),
];

/// The struct an anchored field name (`cfg+0x200`) belongs to.
fn struct_of(anchor: &str) -> Option<&'static str> {
    let prefix = anchor.split('+').next()?;
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == prefix)
        .map(|(_, name)| *name)
}

/// The field table: every anchored field of the five structs at
/// `offsets[i]` (this build's offset of `sites::FIELDS[i]`), plus the
/// unanchored layout. Sorted by offset.
pub fn field_table(offsets: &[usize]) -> BTreeMap<String, Vec<Field>> {
    let mut table: BTreeMap<String, Vec<Field>> = BTreeMap::new();
    for (index, (info, &offset)) in FIELDS.iter().zip(offsets).enumerate() {
        let Some(owner) = struct_of(info.name) else {
            continue;
        };
        let typed = ANCHORED.iter().find(|(i, _, _)| *i == index);
        let name = match (info.reflection, typed) {
            (r, _) if !r.is_empty() => r.to_owned(),
            (_, Some((_, n, _))) => (*n).to_owned(),
            _ => format!("{offset:#x}"),
        };
        table.entry(owner.to_owned()).or_default().push(Field {
            name,
            offset: offset as u64,
            ty: typed.map(|(_, _, t)| *t),
        });
    }
    for &(owner, name, offset, ty) in UNANCHORED {
        table.entry(owner.to_owned()).or_default().push(Field {
            name: name.to_owned(),
            offset: offset as u64,
            ty: Some(ty),
        });
    }
    for fields in table.values_mut() {
        fields.sort_by_key(|f| f.offset);
    }
    table
}

/// Bytes of each engine struct that may be written: its documented object
/// size (TAA filter: at least through its dense-velocity pointer).
pub fn struct_len(name: &str, taa_dense_velocity: usize) -> Option<u64> {
    Some(match name {
        "RenderConfig" => render_config::SIZE as u64,
        "Camera" => camera::SIZE as u64,
        "RenderSettings" => RENDER_SETTINGS_SIZE,
        "TaaFilter" => taa::SIZE.max(taa_dense_velocity + 8) as u64,
        "Device" => device::SIZE as u64,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use mhw_engine::sites::FIELDS;

    use super::*;

    fn book() -> Symbols {
        let mut s = Symbols::default();
        s.add_struct("RenderConfig", 0x1000, 0x100);
        s.add_struct("Device", 0x8000, 0x10);
        s.names.insert("site.CAMERA_SETUP".into(), 0x14228f3b0);
        s
    }

    #[test]
    fn aliases_resolve_to_their_struct() {
        let s = book();
        assert_eq!(s.lookup("cfg"), Some(0x1000));
        assert_eq!(s.lookup("RenderConfig"), Some(0x1000));
        assert_eq!(s.lookup("dev"), Some(0x8000));
        assert_eq!(s.lookup("site.CAMERA_SETUP"), Some(0x14228f3b0));
        assert_eq!(s.lookup("taa"), None, "not resolved yet");
        assert_eq!(s.lookup("CFG"), None, "case-sensitive");
    }

    #[test]
    fn writes_must_fit_inside_one_struct() {
        let s = book();
        assert!(s.check_write(0x1000, 0x100, false).is_ok());
        assert!(s.check_write(0x10ff, 1, false).is_ok());
        let refused = s.check_write(0x10ff, 2, false).unwrap_err();
        assert_eq!(refused.code, crate::proto::ErrorCode::Refused);
        assert!(s.check_write(0xfff, 1, false).is_err());
        assert!(
            s.check_write(0x1000, 0x8000, false).is_err(),
            "spans two structs"
        );
        assert!(s.check_write(u64::MAX, 2, false).is_err(), "wraps");
        assert!(s.check_write(0x5000, 4, true).is_ok(), "force");
        assert_eq!(
            s.region_of(0x8008, 8).map(|r| r.name.as_str()),
            Some("Device")
        );
    }

    #[test]
    fn the_field_table_names_and_types_the_struct_fields() {
        let known: Vec<usize> = FIELDS.iter().map(|f| f.known).collect();
        let table = field_table(&known);
        assert_eq!(table.len(), 5, "{:?}", table.keys());
        let find = |s: &str, n: &str| table[s].iter().find(|f| f.name == n).cloned().unwrap();
        assert_eq!(find("RenderConfig", "live_ratio").offset, 0x200);
        assert_eq!(find("RenderConfig", "mUseDLSS").ty, Some(ValueType::U8));
        assert_eq!(find("Camera", "jitter_x").ty, Some(ValueType::F32));
        assert_eq!(find("TaaFilter", "dense_velocity").offset, 0x1a8);
        assert_eq!(find("Device", "wait_latch").offset, 0x10da);
        // A moved field shows its offset in this build.
        let mut moved = known.clone();
        moved[field::CFG_200] = 0x210;
        assert_eq!(
            field_table(&moved)["RenderConfig"]
                .iter()
                .find(|f| f.name == "live_ratio")
                .unwrap()
                .offset,
            0x210
        );
        for fields in table.values() {
            assert!(fields.windows(2).all(|w| w[0].offset <= w[1].offset));
        }
    }

    #[test]
    fn struct_lengths_cover_the_documented_objects() {
        assert_eq!(struct_len("Device", 0), Some(0x42720));
        assert_eq!(struct_len("TaaFilter", 0x1a8), Some(0x1b0));
        assert_eq!(struct_len("TaaFilter", 0x1c0), Some(0x1c8));
        assert_eq!(struct_len("GameCode", 0), None);
    }

    #[test]
    fn the_json_carries_hex_addresses() {
        let v = book().to_json();
        assert_eq!(v["symbols"]["RenderConfig"], "0x1000");
        assert_eq!(v["aliases"]["cfg"], "RenderConfig");
        assert_eq!(v["regions"][1]["len"], 0x10);
    }
}
