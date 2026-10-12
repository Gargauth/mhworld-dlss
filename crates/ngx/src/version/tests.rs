use super::*;

/// A `VS_VERSIONINFO` block with `file` and `product` versions.
fn version_blob(file: (u32, u32), product: (u32, u32)) -> Vec<u8> {
    let mut blob = Vec::new();
    blob.extend(0u16.to_le_bytes()); // wLength, patched below
    blob.extend((FIXED_LEN as u16).to_le_bytes());
    blob.extend(0u16.to_le_bytes()); // binary
    for unit in "VS_VERSION_INFO\0".encode_utf16() {
        blob.extend(unit.to_le_bytes());
    }
    while blob.len() % 4 != 0 {
        blob.push(0);
    }
    for word in [
        FIXED_SIGNATURE,
        0x0001_0000,
        file.0,
        file.1,
        product.0,
        product.1,
        0x3f,
        0,
        0x0004_0004,
        2,
        0,
        0,
        0,
    ] {
        blob.extend(word.to_le_bytes());
    }
    // A StringFileInfo child would follow; its presence must not matter.
    blob.extend([0xAA; 12]);
    let length = blob.len() as u16;
    blob[..2].copy_from_slice(&length.to_le_bytes());
    blob
}

/// 310.4.0.0
const DLSS_310_4: (u32, u32) = (310 << 16 | 4, 0);

#[test]
fn the_fixed_file_info_gives_both_versions() {
    let blob = version_blob(DLSS_310_4, (310 << 16 | 4, 1 << 16 | 2));
    let info = parse_version_info(&blob).unwrap();
    assert_eq!(info.file.to_string(), "310.4.0.0");
    assert_eq!(info.product.to_string(), "310.4.1.2");
    assert!(info.file < info.product);
}

#[test]
fn malformed_blocks_are_rejected() {
    let good = version_blob(DLSS_310_4, DLSS_310_4);
    // Wrong key.
    let mut key = good.clone();
    key[6] = b'X';
    assert_eq!(parse_version_info(&key), None);
    // Wrong signature.
    let mut signature = good.clone();
    signature[40] ^= 1;
    assert_eq!(parse_version_info(&signature), None);
    // No fixed info declared.
    let mut empty = good.clone();
    empty[2..4].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(parse_version_info(&empty), None);
    // Truncated.
    assert_eq!(parse_version_info(&good[..50]), None);
    assert_eq!(parse_version_info(&[]), None);
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend(value.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend(value.to_le_bytes());
}

/// A directory header with `entries` (id, data) entries.
fn directory(out: &mut Vec<u8>, named: u16, entries: &[(u32, u32)]) {
    push_u32(out, 0);
    push_u32(out, 0);
    push_u16(out, 4);
    push_u16(out, 0);
    push_u16(out, named);
    push_u16(out, entries.len() as u16 - named);
    for (name, data) in entries {
        push_u32(out, *name);
        push_u32(out, *data);
    }
}

/// A `.rsrc` section at `base` holding an icon type (3) before the version
/// type (16), whose blob is `blob`.
fn rsrc(base: u32, blob: &[u8]) -> Vec<u8> {
    const SUB: u32 = HIGH_BIT;
    let mut out = Vec::new();
    // Root: 0x00, 2 entries -> 0x20 bytes.
    directory(&mut out, 0, &[(3, SUB | 0x200), (RT_VERSION, SUB | 0x20)]);
    // Names for RT_VERSION at 0x20: one id entry (1) -> 0x18 bytes.
    directory(&mut out, 0, &[(1, SUB | 0x38)]);
    // Languages at 0x38: one (0x409) -> data entry at 0x50.
    directory(&mut out, 0, &[(0x409, 0x50)]);
    assert_eq!(out.len(), 0x50);
    // Data entry: RVA of the blob at 0x60, its size.
    push_u32(&mut out, base + 0x60);
    push_u32(&mut out, blob.len() as u32);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    out.resize(0x60, 0);
    out.extend(blob);
    // The icon subtree at 0x200 is never followed.
    out.resize(0x200, 0);
    directory(&mut out, 0, &[]);
    out
}

#[test]
fn the_version_resource_is_found_through_the_tree() {
    let blob = version_blob(DLSS_310_4, DLSS_310_4);
    let mut image = vec![0u8; 0x100];
    image.extend(rsrc(0x100, &blob));
    let directory = DataDirectory {
        rva: 0x100,
        size: (image.len() - 0x100) as u32,
    };
    assert_eq!(
        version_resource(image.as_slice(), directory),
        Some(blob.as_slice())
    );
    assert_eq!(
        version_resource(image.as_slice(), DataDirectory { rva: 0, size: 0 }),
        None
    );
}

#[test]
fn an_image_without_a_version_type_has_no_version() {
    let mut image = vec![0u8; 0x10];
    directory(&mut image, 0, &[(3, HIGH_BIT | 0x18)]);
    directory(&mut image, 0, &[]);
    let resources = DataDirectory {
        rva: 0x10,
        size: 0x30,
    };
    assert_eq!(version_resource(image.as_slice(), resources), None);
}

const HEADERS: usize = 0x200;
const SECTION_RVA: u32 = 0x1000;

/// A minimal PE32+ file with one `.rsrc` section, and the same image as
/// the loader would map it.
fn tiny_pe(blob: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let section = rsrc(SECTION_RVA, blob);
    let mut headers = vec![0u8; HEADERS];
    headers[..2].copy_from_slice(b"MZ");
    headers[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    headers[0x40..0x44].copy_from_slice(b"PE\0\0");
    let file = 0x44;
    headers[file..file + 2].copy_from_slice(&0x8664u16.to_le_bytes());
    headers[file + 2..file + 4].copy_from_slice(&1u16.to_le_bytes());
    headers[file + 16..file + 18].copy_from_slice(&240u16.to_le_bytes());
    let optional = file + 20;
    let put32 = |headers: &mut Vec<u8>, at: usize, value: u32| {
        headers[at..at + 4].copy_from_slice(&value.to_le_bytes())
    };
    headers[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
    let image_size = SECTION_RVA + section.len() as u32;
    put32(&mut headers, optional + 56, image_size);
    put32(&mut headers, optional + 60, HEADERS as u32);
    put32(&mut headers, optional + 108, 16);
    put32(&mut headers, optional + 112 + 8 * 2, SECTION_RVA);
    put32(
        &mut headers,
        optional + 112 + 8 * 2 + 4,
        section.len() as u32,
    );
    let table = optional + 240;
    headers[table..table + 5].copy_from_slice(b".rsrc");
    put32(&mut headers, table + 8, section.len() as u32);
    put32(&mut headers, table + 12, SECTION_RVA);
    put32(&mut headers, table + 16, section.len() as u32);
    put32(&mut headers, table + 20, HEADERS as u32);

    let mut file_bytes = headers.clone();
    file_bytes.extend(&section);
    let mut mapped = headers;
    mapped.resize(SECTION_RVA as usize, 0);
    mapped.extend(&section);
    (file_bytes, mapped)
}

#[test]
fn a_file_and_its_mapped_image_give_the_same_version() {
    let blob = version_blob(DLSS_310_4, (310 << 16 | 4, 7));
    let (file, mapped) = tiny_pe(&blob);
    let from_file = file_version(&file).unwrap();
    assert_eq!(from_file.file.to_string(), "310.4.0.0");
    assert_eq!(from_file.product.to_string(), "310.4.0.7");
    let headers = pe::parse_headers(mapped.as_slice()).unwrap();
    assert_eq!(image_version(mapped.as_slice(), &headers), Some(from_file));
}

#[test]
fn file_offsets_follow_the_section_table() {
    let blob = version_blob(DLSS_310_4, DLSS_310_4);
    let (file, _) = tiny_pe(&blob);
    let image = FileImage::parse(&file).unwrap();
    // Headers in place, the section at its raw offset, nothing past it.
    assert_eq!(image.bytes(0, 2), Some(&b"MZ"[..]));
    assert_eq!(
        image.bytes(SECTION_RVA, 4),
        Some(&file[HEADERS..HEADERS + 4])
    );
    let past = SECTION_RVA + (file.len() - HEADERS) as u32;
    assert_eq!(image.bytes_at(past, 1), None);
    // A read is cut at the end of its section.
    assert_eq!(image.bytes_at(past - 3, 100).map(<[u8]>::len), Some(3));
}

#[test]
fn non_pe_bytes_have_no_version() {
    assert_eq!(file_version(b"not a PE file at all"), None);
    assert_eq!(file_version(&[]), None);
}

#[test]
fn a_file_on_disk_is_read_sparsely() {
    let blob = version_blob(DLSS_310_4, DLSS_310_4);
    let (file, _) = tiny_pe(&blob);
    let dir = std::env::temp_dir().join(format!("mhw-ngx-version-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pe = dir.join("nvngx_dlss.dll");
    std::fs::write(&pe, &file).unwrap();
    let text = dir.join("readme.txt");
    std::fs::write(&text, b"plain text").unwrap();

    let version = read_file_version(&pe).unwrap().unwrap();
    assert_eq!(version.file.to_string(), "310.4.0.0");
    assert_eq!(read_file_version(&text).unwrap(), None);
    assert!(read_file_version(&dir.join("absent.dll")).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
