//! Video preview frames through Windows Media Foundation.
//!
//! Adapted from oddity-picview `src/fileops.rs::video_frames`.

use std::path::Path;

pub const EXTENSIONS: [&str; 15] = [
    "mp4", "m4v", "mkv", "avi", "mov", "wmv", "webm", "flv", "mpg", "mpeg", "ts", "mts", "m2ts",
    "3gp", "asf",
];

pub fn is_video(name: &str) -> bool {
    has_ext(name, &EXTENSIONS)
}

pub const IMAGE_EXTENSIONS: [&str; 14] = [
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "ico", "heic", "heif", "avif", "jfif", "svg",
];

fn has_ext(name: &str, list: &[&str]) -> bool {
    name.rsplit_once('.')
        .map(|(_, ext)| list.iter().any(|e| e.eq_ignore_ascii_case(ext)))
        .unwrap_or(false)
}

/// Video or picture: double-click opens it in the default player or viewer.
pub fn is_media(name: &str) -> bool {
    is_video(name) || has_ext(name, &IMAGE_EXTENSIONS)
}

/// Upper bound for one video, so a file with far-apart keyframes cannot hold a worker.
const TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(8);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `n` frames spread over a video (at the middle of each of n equal parts), each
/// scaled to `width` pixels wide. Uses Windows' own decoders, so it reads what the
/// Films & TV app reads. Blocking; run it on a worker thread.
/// Gives up when `keep_going` turns false (the pointer moved on) or after
/// `TIME_LIMIT`, keeping the frames made so far.
pub fn video_frames(
    path: &Path,
    n: usize,
    width: u32,
    keep_going: &dyn Fn() -> bool,
) -> Result<Vec<image::RgbaImage>, String> {
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::System::Variant::VT_I8;

    let wpath = wide(&path.to_string_lossy().replace('/', "\\"));
    unsafe {
        // worker thread: join the multithreaded COM apartment, start Media Foundation
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| e.to_string())?;
        let res = (|| -> windows::core::Result<Vec<image::RgbaImage>> {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 2)?;
            let attrs = attrs.ok_or_else(windows::core::Error::empty)?;
            // the reader converts to RGB32 and scales down itself, and may use the GPU decoder
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            let reader = MFCreateSourceReaderFromURL(PCWSTR(wpath.as_ptr()), &attrs)?;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(stream, true)?;
            let native = reader.GetNativeMediaType(stream, 0)?;
            let ns = native.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let (nw, nh) = ((ns >> 32) as u32, (ns & 0xffff_ffff) as u32);
            let want = MFCreateMediaType()?;
            want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            if nw > width && nh > 0 {
                let h = ((width as u64 * nh as u64) / nw as u64).max(2) as u32 & !1;
                want.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | h as u64)?;
            }
            reader.SetCurrentMediaType(stream, None, &want)?;
            let cur = reader.GetCurrentMediaType(stream)?;
            let fs = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let (fw, fh) = ((fs >> 32) as u32, (fs & 0xffff_ffff) as u32);
            if fw == 0 || fh == 0 {
                return Err(windows::core::Error::empty());
            }
            let stride = cur
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| s as i32)
                .unwrap_or(fw as i32 * 4);
            let dur_pv =
                reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)?;
            let dur = dur_pv.Anonymous.Anonymous.Anonymous.uhVal; // 100 ns units
            let height = ((width as u64 * fh as u64) / fw as u64).max(1) as u32;
            // phones store portrait video sideways plus a rotation tag (degrees)
            let rotation = native.GetUINT32(&MF_MT_VIDEO_ROTATION).unwrap_or(0);
            let started = std::time::Instant::now();

            let mut out = Vec::new();
            for k in 0..n {
                if !keep_going() {
                    return Err(windows::core::Error::empty());
                }
                if started.elapsed() > TIME_LIMIT {
                    break;
                }
                let mut at = 0i64;
                if dur > 0 {
                    at = (dur as f64 * (k as f64 + 0.5) / n as f64) as i64;
                    let mut pos = PROPVARIANT::default();
                    (*pos.Anonymous.Anonymous).vt = VT_I8;
                    (*pos.Anonymous.Anonymous).Anonymous.hVal = at;
                    reader.SetCurrentPosition(&GUID::zeroed(), &pos)?;
                }
                // seeking lands on the keyframe before `at`: decode forward to `at`
                // (at most 400 frames), or all frames would be the same keyframe
                let mut sample: Option<IMFSample> = None;
                for step in 0..400 {
                    if step % 16 == 15 && (!keep_going() || started.elapsed() > TIME_LIMIT) {
                        break;
                    }
                    let mut flags = 0u32;
                    let mut ts = 0i64;
                    let mut got: Option<IMFSample> = None;
                    reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut got))?;
                    if got.is_some() {
                        sample = got;
                    }
                    if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 || (sample.is_some() && ts >= at) {
                        break;
                    }
                }
                let Some(sample) = sample else { break };
                let buf = sample.ConvertToContiguousBuffer()?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut ptr, None, Some(&mut len))?;
                let data = std::slice::from_raw_parts(ptr, len as usize);
                let row = stride.unsigned_abs() as usize;
                let mut img = image::RgbaImage::new(fw, fh);
                for y in 0..fh as usize {
                    // negative stride = bottom-up rows
                    let src_y = if stride < 0 { fh as usize - 1 - y } else { y };
                    let Some(src) = data.get(src_y * row..src_y * row + fw as usize * 4) else { break };
                    for (x, px) in src.chunks_exact(4).enumerate() {
                        img.put_pixel(x as u32, y as u32, image::Rgba([px[2], px[1], px[0], 255])); // BGRX
                    }
                }
                let _ = buf.Unlock();
                let img = if fw == width {
                    img
                } else {
                    image::imageops::resize(&img, width, height, image::imageops::FilterType::Triangle)
                };
                out.push(match rotation {
                    90 => image::imageops::rotate90(&img),
                    180 => image::imageops::rotate180(&img),
                    270 => image::imageops::rotate270(&img),
                    _ => img,
                });
                if dur == 0 {
                    break; // no length: the first frame is all we can get
                }
            }
            Ok(out)
        })();
        let _ = MFShutdown();
        match res {
            Ok(v) if !v.is_empty() => Ok(v),
            Ok(_) => Err("프레임이 없습니다".into()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_media, is_video};

    #[test]
    fn video_names() {
        assert!(is_video("a.MP4"));
        assert!(is_video("movie.final.mkv"));
        assert!(!is_video("mp4"));
        assert!(!is_video("photo.jpg"));
        assert!(is_media("photo.JPG"));
        assert!(is_media("clip.mkv"));
        assert!(!is_media("setup.exe"));
    }

    /// DISKMAP_VIDEO=<file> [DISKMAP_STRIP=<out.png>] cargo test --release frames -- --ignored --nocapture
    #[test]
    #[ignore]
    fn frames() {
        let path = std::env::var("DISKMAP_VIDEO").expect("set DISKMAP_VIDEO");
        let t = std::time::Instant::now();
        let v = super::video_frames(std::path::Path::new(&path), 10, 320, &|| true).expect("frames");
        println!("{} frames {}x{} in {:?}", v.len(), v[0].width(), v[0].height(), t.elapsed());
        assert_eq!(v.len(), 10);
        assert_ne!(v[0].as_raw(), v[9].as_raw(), "first and last frame must differ");
        if let Ok(out) = std::env::var("DISKMAP_STRIP") {
            let mut strip = image::RgbaImage::new(320 * 5, v[0].height() * 2);
            for (k, f) in v.iter().enumerate() {
                image::imageops::replace(&mut strip, f, (k % 5) as i64 * 320, (k / 5) as i64 * v[0].height() as i64);
            }
            strip.save(out).unwrap();
        }
    }
}
