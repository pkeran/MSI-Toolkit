//! Self-contained PE-overlay archive for single-file `MSI-Toolkit-Setup.exe`.
//!
//! Layout appended to the compiled `msilt-setup.exe` stub:
//!   [PE stub bytes: 0..payload_offset]
//!   [Payload:
//!       magic: b"MSIPAK01" (8 bytes)
//!       count: u32 LE
//!       for each entry:
//!           name_len: u16 LE
//!           name: UTF-8 allowlisted filename
//!           orig_len: u64 LE
//!           comp_len: u64 LE
//!           crc32: u32 LE
//!           comp_bytes: raw DEFLATE stream
//!   ]
//!   [Trailer (24 bytes):
//!       payload_offset: u64 LE
//!       payload_len: u64 LE
//!       magic: b"MSITAIL1" (8 bytes)
//!   ]

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;

pub const BUNDLE_FILES: &[&str] = &[
    "msilt-daemon.exe",
    "msilt-ui.exe",
    "msilt-cli.exe",
    "msilt-probe.exe",
];

const PAYLOAD_MAGIC: &[u8; 8] = b"MSIPAK01";
const TRAILER_MAGIC: &[u8; 8] = b"MSITAIL1";
const TRAILER_LEN: u64 = 24;
const MAX_SINGLE_FILE: u64 = 48 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayMeta {
    pub payload_offset: u64,
    pub payload_len: u64,
}

#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub name: String,
    pub orig_len: u64,
    pub comp_len: u64,
    pub crc32: u32,
    pub data_offset: u64,
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

pub fn is_allowlisted_binary(name: &str) -> bool {
    BUNDLE_FILES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// Inspects an executable to see whether it carries a valid MSIPAK01 overlay.
pub fn inspect_overlay(exe_path: &Path) -> io::Result<Option<OverlayMeta>> {
    let mut file = File::open(exe_path)?;
    let total = file.metadata()?.len();
    if total < TRAILER_LEN + 12 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(total - TRAILER_LEN))?;
    let mut trailer = [0u8; 24];
    file.read_exact(&mut trailer)?;
    if &trailer[16..24] != TRAILER_MAGIC {
        return Ok(None);
    }
    let payload_offset = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
    let payload_len = u64::from_le_bytes(trailer[8..16].try_into().unwrap());
    if payload_offset == 0
        || payload_len < 12
        || payload_offset
            .checked_add(payload_len)
            .and_then(|end| end.checked_add(TRAILER_LEN))
            != Some(total)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "安装包内部索引校验失败",
        ));
    }
    file.seek(SeekFrom::Start(payload_offset))?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != PAYLOAD_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "安装包数据头无效",
        ));
    }
    Ok(Some(OverlayMeta {
        payload_offset,
        payload_len,
    }))
}

/// Packs `BUNDLE_FILES` from `source_dir` onto the PE stub `stub_exe`,
/// writing the single-file installer to `output_exe`.
pub fn pack_installer(stub_exe: &Path, source_dir: &Path, output_exe: &Path) -> io::Result<(u64, u64)> {
    // Strip any existing overlay if stub_exe itself already had one.
    let stub_len = match inspect_overlay(stub_exe)? {
        Some(meta) => meta.payload_offset,
        None => fs::metadata(stub_exe)?.len(),
    };
    let mut stub_bytes = vec![0u8; stub_len as usize];
    File::open(stub_exe)?.read_exact(&mut stub_bytes)?;

    let mut payload = Vec::new();
    payload.extend_from_slice(PAYLOAD_MAGIC);
    payload.extend_from_slice(&(BUNDLE_FILES.len() as u32).to_le_bytes());

    let mut total_unpacked: u64 = 0;
    for &name in BUNDLE_FILES {
        let src = source_dir.join(name);
        let raw = fs::read(&src).map_err(|e| {
            io::Error::new(e.kind(), format!("缺少程序文件 {} ({e})", src.display()))
        })?;
        total_unpacked += raw.len() as u64;
        let checksum = crc32(&raw);
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&raw)?;
        let compressed = encoder.finish()?;

        let name_bytes = name.as_bytes();
        payload.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        payload.extend_from_slice(name_bytes);
        payload.extend_from_slice(&(raw.len() as u64).to_le_bytes());
        payload.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
        payload.extend_from_slice(&checksum.to_le_bytes());
        payload.extend_from_slice(&compressed);
    }

    if let Some(parent) = output_exe.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = File::create(output_exe)?;
    out.write_all(&stub_bytes)?;
    out.write_all(&payload)?;
    out.write_all(&stub_len.to_le_bytes())?;
    out.write_all(&(payload.len() as u64).to_le_bytes())?;
    out.write_all(TRAILER_MAGIC)?;
    out.sync_all()?;

    let final_size = stub_len + payload.len() as u64 + TRAILER_LEN;
    Ok((total_unpacked, final_size))
}

/// Writes ONLY the small PE stub (without the embedded payload overlay) to
/// `dest_path`, used to install a lightweight ~250 KiB `uninstall.exe`.
pub fn write_stub_copy(current_exe: &Path, dest_path: &Path) -> io::Result<u64> {
    let stub_len = match inspect_overlay(current_exe)? {
        Some(meta) => meta.payload_offset,
        None => fs::metadata(current_exe)?.len(),
    };
    let mut src = File::open(current_exe)?;
    let mut limited = std::io::Read::by_ref(&mut src).take(stub_len);
    let mut out = File::create(dest_path)?;
    let copied = io::copy(&mut limited, &mut out)?;
    if copied != stub_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "复制卸载程序存根失败",
        ));
    }
    out.sync_all()?;
    Ok(copied)
}

/// Parses entry metadata inside an embedded overlay.
pub fn list_entries(exe_path: &Path, meta: OverlayMeta) -> io::Result<Vec<ArchiveEntry>> {
    let mut file = File::open(exe_path)?;
    let payload_end = meta.payload_offset + meta.payload_len;
    file.seek(SeekFrom::Start(meta.payload_offset + 8))?;

    let count = read_u32(&mut file)?;
    if count == 0 || count > 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "安装包条目数量异常",
        ));
    }

    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name_len = read_u16(&mut file)? as usize;
        if name_len == 0 || name_len > 128 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "无效的文件名长度"));
        }
        let mut name_buf = vec![0u8; name_len];
        file.read_exact(&mut name_buf)?;
        let name = String::from_utf8(name_buf)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "文件名不是合法 UTF-8"))?;
        if !is_allowlisted_binary(&name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("安装包包含非白名单文件：{name}"),
            ));
        }
        let orig_len = read_u64(&mut file)?;
        let comp_len = read_u64(&mut file)?;
        let checksum = read_u32(&mut file)?;
        if orig_len == 0 || orig_len > MAX_SINGLE_FILE || comp_len > MAX_SINGLE_FILE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("文件体积超出安全上限：{name}"),
            ));
        }
        let data_offset = file.stream_position()?;
        let next_pos = data_offset
            .checked_add(comp_len)
            .filter(|&p| p <= payload_end)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "安装包数据截断"))?;
        file.seek(SeekFrom::Start(next_pos))?;
        entries.push(ArchiveEntry {
            name,
            orig_len,
            comp_len,
            crc32: checksum,
            data_offset,
        });
    }
    Ok(entries)
}

/// Decompresses and CRC32-verifies a single archive entry.
pub fn read_entry_bytes(exe_path: &Path, entry: &ArchiveEntry) -> io::Result<Vec<u8>> {
    let mut file = File::open(exe_path)?;
    file.seek(SeekFrom::Start(entry.data_offset))?;
    let mut comp = vec![0u8; entry.comp_len as usize];
    file.read_exact(&mut comp)?;

    let mut decoder = DeflateDecoder::new(&comp[..]);
    let mut out = Vec::with_capacity(entry.orig_len as usize);
    decoder
        .by_ref()
        .take(entry.orig_len + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 != entry.orig_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("解压 {} 长度不匹配", entry.name),
        ));
    }
    if crc32(&out) != entry.crc32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("文件 {} 完整性校验失败（CRC32 不匹配）", entry.name),
        ));
    }
    Ok(out)
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_inspect_extract_and_detect_corruption() {
        let dir = std::env::temp_dir().join(format!("msilt-setup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let stub = dir.join("stub.exe");
        let stub_data = b"MZ_FAKE_STUB_HEADER_BYTES_1234567890";
        fs::write(&stub, stub_data).unwrap();

        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        for (idx, &name) in BUNDLE_FILES.iter().enumerate() {
            let content = vec![(0x30 + idx) as u8; 4096 + idx * 137];
            fs::write(src.join(name), &content).unwrap();
        }

        let packed = dir.join("MSI-Toolkit-Setup.exe");
        let (unpacked, final_len) = pack_installer(&stub, &src, &packed).unwrap();
        assert!(unpacked > 16_000);
        assert!(final_len < unpacked);

        let meta = inspect_overlay(&packed).unwrap().expect("overlay present");
        assert_eq!(meta.payload_offset, stub_data.len() as u64);

        let entries = list_entries(&packed, meta).unwrap();
        assert_eq!(entries.len(), BUNDLE_FILES.len());
        for (idx, entry) in entries.iter().enumerate() {
            let bytes = read_entry_bytes(&packed, entry).unwrap();
            assert_eq!(bytes.len(), 4096 + idx * 137);
            assert!(bytes.iter().all(|&b| b == (0x30 + idx) as u8));
        }

        // Stub copy strips the payload overlay cleanly.
        let uninst = dir.join("uninstall.exe");
        let copied = write_stub_copy(&packed, &uninst).unwrap();
        assert_eq!(copied, stub_data.len() as u64);
        assert_eq!(fs::read(&uninst).unwrap(), stub_data);
        assert!(inspect_overlay(&uninst).unwrap().is_none());

        // Flipping one byte inside compressed data triggers CRC32 / deflate rejection.
        let mut tampered = fs::read(&packed).unwrap();
        let target_idx = entries[0].data_offset as usize + 2;
        tampered[target_idx] ^= 0xFF;
        fs::write(&packed, &tampered).unwrap();
        assert!(read_entry_bytes(&packed, &entries[0]).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
