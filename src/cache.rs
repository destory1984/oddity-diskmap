//! On-disk cache for video preview frames.
//!
//! Adapted from oddity-picview `src/cache.rs`.
//! Layout: %LOCALAPPDATA%\oddity-diskmap\thumbs\<2 hex>\<16 hex>.jpg|.png
//! The key hashes the full path, file size and mtime, so an edited file gets a
//! new entry and the stale one ages out through `trim`.

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

/// Default size limit, changeable in the settings window.
pub const DEFAULT_MAX_MB: u64 = 100;
pub const MIN_MB: u64 = 50;
pub const MAX_MB: u64 = 1024;
/// Bump when the stored format or frame size changes, to ignore old entries.
const VERSION: u64 = 1;

pub fn dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        // DISKMAP_CACHE_DIR lets tests run against a throwaway folder
        let d = match std::env::var_os("DISKMAP_CACHE_DIR") {
            Some(d) => PathBuf::from(d),
            None => PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("oddity-diskmap").join("thumbs"),
        };
        std::fs::create_dir_all(&d).ok()?;
        Some(d)
    })
    .as_deref()
}

/// FNV-1a: stable across builds, unlike std's DefaultHasher.
fn fnv(data: &[u8], mut h: u64) -> u64 {
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// `mtime` is the FILETIME the scanner recorded.
fn key(name: &str, size: u64, mtime: u64) -> u64 {
    let mut h = fnv(name.to_lowercase().as_bytes(), 0xcbf29ce484222325);
    h = fnv(&size.to_le_bytes(), h);
    h = fnv(&mtime.to_le_bytes(), h);
    fnv(&VERSION.to_le_bytes(), h)
}

fn entry_base(name: &str, size: u64, mtime: u64) -> Option<PathBuf> {
    let k = format!("{:016x}", key(name, size, mtime));
    Some(dir()?.join(&k[..2]).join(k))
}

pub fn load(name: &str, size: u64, mtime: u64) -> Option<RgbaImage> {
    use std::io::Read;
    let base = entry_base(name, size, mtime)?;
    for ext in ["jpg", "png"] {
        let Ok(mut f) = std::fs::File::options().read(true).write(true).open(base.with_extension(ext)) else {
            continue;
        };
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).ok()?;
        // keep recently used entries young so `trim` drops cold ones first
        let old = f
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|e| e.as_secs() > 86_400).unwrap_or(false))
            .unwrap_or(false);
        if old {
            let _ = f.set_modified(SystemTime::now());
        }
        return image::load_from_memory(&bytes).ok().map(|i| i.to_rgba8());
    }
    None
}

/// Opaque images are stored as JPEG (small), ones with alpha as PNG.
pub fn store(name: &str, size: u64, mtime: u64, img: &RgbaImage) {
    let Some(base) = entry_base(name, size, mtime) else { return };
    let (w, h) = img.dimensions();
    let opaque = img.pixels().all(|p| p[3] == 255);
    let mut buf = Vec::new();
    let (ext, ok) = if opaque {
        let rgb: Vec<u8> = img.pixels().flat_map(|p| [p[0], p[1], p[2]]).collect();
        let r = JpegEncoder::new_with_quality(&mut buf, 88).write_image(&rgb, w, h, ExtendedColorType::Rgb8);
        ("jpg", r.is_ok())
    } else {
        let r = PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::Sub)
            .write_image(img.as_raw(), w, h, ExtendedColorType::Rgba8);
        ("png", r.is_ok())
    };
    if !ok {
        return;
    }
    let target = base.with_extension(ext);
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // write then rename, so a crash never leaves a half-written entry
    let tmp = target.with_extension(format!("{ext}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &buf).is_ok() && std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Total size of the cache in bytes.
pub fn usage() -> u64 {
    let Some(d) = dir() else { return 0 };
    std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|shard| std::fs::read_dir(shard.path()).into_iter().flatten().flatten())
        .filter_map(|f| f.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// When the cache is over `max_bytes`, delete the least recently used entries
/// until it is down to 80% of it. Run off the UI thread.
pub fn trim(max_bytes: u64) {
    let trim_to = max_bytes / 10 * 8;
    let Some(d) = dir() else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for shard in std::fs::read_dir(d).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            let Ok(md) = f.metadata() else { continue };
            if f.path().extension().is_some_and(|e| e == "tmp") {
                let _ = std::fs::remove_file(f.path()); // leftovers from a crash
                continue;
            }
            total += md.len();
            files.push((md.modified().unwrap_or(SystemTime::UNIX_EPOCH), md.len(), f.path()));
        }
    }
    if total <= max_bytes {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, p) in files {
        if total <= trim_to {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}

pub fn clear() -> std::io::Result<()> {
    if let Some(d) = dir() {
        for shard in std::fs::read_dir(d)?.flatten() {
            let _ = std::fs::remove_dir_all(shard.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_trim() {
        let d = std::env::temp_dir().join(format!("diskmap-cache-{}", std::process::id()));
        // SAFETY: set before dir() is first called in this test binary; tests touching it run here only
        unsafe { std::env::set_var("DISKMAP_CACHE_DIR", &d) };
        let img = RgbaImage::from_pixel(40, 30, image::Rgba([10, 200, 30, 255]));
        store("C:\\v\\a.mp4#clip10", 1, 7, &img);
        let back = load("c:\\V\\A.mp4#clip10", 1, 7).expect("case-insensitive hit");
        assert_eq!(back.dimensions(), (40, 30));
        assert!(load("C:\\v\\a.mp4#clip10", 1, 8).is_none(), "changed mtime must miss");
        assert!(usage() > 0);
        trim(0);
        assert_eq!(usage(), 0);
        let _ = std::fs::remove_dir_all(d);
    }
}
