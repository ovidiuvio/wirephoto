//! What the server reads from a photo to serve it (design.md, Scope): kind and MIME type from the content, the
//! displayed size, the EXIF orientation, the capture time, and whether a preview can be made.

pub mod heif;
pub mod jpeg;
pub mod rawjpeg;
pub mod tiff;

use std::io;

use wirephoto_api::Kind;

use crate::kind;
use rawjpeg::ReadAt;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub kind: Option<Kind>,
    pub mime: Option<String>,
    /// Displayed size: orientation already applied.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub orientation: Option<u8>,
    /// RFC 3339 when the offset is known; local date-time without an offset otherwise.
    pub taken: Option<String>,
    /// Seconds since the epoch for sorting (the local time read as UTC when there is no offset).
    pub taken_ts: Option<i64>,
    pub duration: Option<f64>,
    pub previewable: bool,
}

/// Formats the server can decode into a preview.
pub fn decodable(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/bmp" | "image/tiff"
    ) || (cfg!(feature = "heic") && matches!(mime, "image/heic" | "image/heif" | "image/avif"))
}

/// Fills in a video's facts (duration, displayed size) from ffprobe in a decode worker; a video gets a poster
/// when it has a picture.
pub fn add_video_facts(f: &mut Facts, pool: &crate::decode::Pool, file: std::fs::File) {
    if f.kind != Some(Kind::Video) {
        return;
    }
    if let Some(p) = pool.probe(file) {
        f.duration = p.duration;
        f.width = p.width;
        f.height = p.height;
        f.orientation = Some(1);
        f.previewable = p.width.is_some();
    }
}

/// "2026:10:12 09:14:03" (+ "+01:00") -> ("2026-10-12T09:14:03+01:00", epoch seconds).
pub fn exif_time(dt: &str, offset: Option<&str>) -> Option<(String, i64)> {
    let naive = chrono::NaiveDateTime::parse_from_str(dt.get(..19)?, "%Y:%m:%d %H:%M:%S").ok()?;
    let iso = naive.format("%Y-%m-%dT%H:%M:%S").to_string();
    if let Some(off) =
        offset.and_then(|o| chrono::DateTime::parse_from_rfc3339(&format!("{iso}{o}")).ok())
    {
        return Some((
            off.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            off.timestamp(),
        ));
    }
    Some((iso, naive.and_utc().timestamp()))
}

fn oriented(w: u32, h: u32, orientation: u8) -> (u32, u32) {
    if (5..=8).contains(&orientation) {
        (h, w)
    } else {
        (w, h)
    }
}

fn apply_exif(f: &mut Facts, x: &tiff::Exif) {
    if let Some((iso, ts)) = x
        .taken
        .as_deref()
        .and_then(|t| exif_time(t, x.offset.as_deref()))
    {
        f.taken = Some(iso);
        f.taken_ts = Some(ts);
    }
    if f.orientation.is_none() {
        f.orientation = x.orientation;
    }
}

/// Reads the facts of a file. `src` is the file (or its bytes, for uploads still in memory).
pub fn read_facts(name: &str, src: &(impl ReadAt + ?Sized)) -> io::Result<Facts> {
    let head = src.read_at(0, rawjpeg::HEAD as u64)?;
    let (kind, mime) = kind::sniff(name, &head);
    let mut f = Facts {
        kind: Some(kind),
        mime: mime.clone(),
        ..Facts::default()
    };
    let Some(mime) = mime else { return Ok(f) };
    match kind {
        Kind::Raw => raw_facts(&mut f, src, &head)?,
        Kind::Image => image_facts(&mut f, &mime, src, &head)?,
        _ => {}
    }
    Ok(f)
}

fn raw_facts(f: &mut Facts, src: &(impl ReadAt + ?Sized), head: &[u8]) -> io::Result<()> {
    let embedded = rawjpeg::find_embedded(src)?;
    // EXIF: the RAW's own TIFF structure, else the embedded JPEG's (RAF, CR3).
    let mut exif = tiff::read_exif(head);
    if exif.as_ref().is_none_or(|x| x.taken.is_none())
        && let Some(e) = &embedded
    {
        let probe = src.read_at(e.offset, e.offset + e.length.min(1 << 18))?;
        if let Some(x) = jpeg::exif(&probe).and_then(tiff::read_exif) {
            exif = Some(x);
        }
    }
    if exif.as_ref().is_none_or(|x| x.taken.is_none()) {
        // CR3: the Exif IFD is the CMT2 box.
        if let Some(i) = head.windows(4).position(|w| w == b"CMT2")
            && let Some(x) = tiff::read_exif(&head[i + 4..]).filter(|x| x.taken.is_some())
        {
            exif = Some(tiff::Exif {
                orientation: exif.as_ref().and_then(|e| e.orientation),
                ..x
            });
        }
    }
    if let Some(x) = &exif {
        apply_exif(f, x);
    }
    let orientation = embedded
        .map(|e| e.orientation)
        .or(f.orientation)
        .unwrap_or(1);
    f.orientation = Some(orientation);
    // The sensor image's size, else the preview's: the aspect ratio is what layout needs.
    let size = tiff::largest_ifd_size(head)
        .or_else(|| exif.as_ref().and_then(|x| Some((x.width?, x.height?))))
        .or_else(|| embedded.map(|e| (e.width, e.height)));
    if let Some((w, h)) = size {
        let (w, h) = oriented(w, h, orientation);
        f.width = Some(w);
        f.height = Some(h);
    }
    f.previewable = embedded.is_some();
    Ok(())
}

fn image_facts(
    f: &mut Facts,
    mime: &str,
    src: &(impl ReadAt + ?Sized),
    head: &[u8],
) -> io::Result<()> {
    match mime {
        "image/jpeg" => {
            let mut info = jpeg::info(head);
            let mut bytes = head.to_vec();
            if info.is_none() && src.size() > head.len() as u64 {
                bytes = src.read_at(0, rawjpeg::SCAN as u64)?; // large APP segments before SOF
                info = jpeg::info(&bytes);
            }
            if let Some(x) = jpeg::exif(&bytes).and_then(tiff::read_exif) {
                apply_exif(f, &x);
            }
            if let Some(i) = info {
                let o = if i.orientation != 0 { i.orientation } else { 1 };
                f.orientation = Some(o);
                let (w, h) = oriented(i.width, i.height, o);
                f.width = Some(w);
                f.height = Some(h);
                f.previewable = true;
            }
        }
        "image/tiff" => {
            if let Some(x) = tiff::read_exif(head) {
                apply_exif(f, &x);
            }
            let o = f.orientation.unwrap_or(1);
            f.orientation = Some(o);
            if let Some((w, h)) = tiff::largest_ifd_size(head) {
                let (w, h) = oriented(w, h, o);
                f.width = Some(w);
                f.height = Some(h);
                f.previewable = true;
            }
        }
        "image/heic" | "image/heif" | "image/avif" => heif_facts(f, src)?,
        "image/png" | "image/gif" | "image/webp" | "image/bmp" => {
            let fmt = match mime {
                "image/png" => image::ImageFormat::Png,
                "image/gif" => image::ImageFormat::Gif,
                "image/webp" => image::ImageFormat::WebP,
                _ => image::ImageFormat::Bmp,
            };
            let reader = image::ImageReader::with_format(io::Cursor::new(head), fmt);
            if let Ok((w, h)) = reader.into_dimensions() {
                f.width = Some(w);
                f.height = Some(h);
                f.orientation = Some(1);
                f.previewable = true;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(feature = "heic")]
fn heif_facts(f: &mut Facts, src: &(impl ReadAt + ?Sized)) -> io::Result<()> {
    let bytes = src.read_at(0, src.size())?;
    let Ok(ctx) = libheif_rs::HeifContext::read_from_bytes(&bytes) else {
        return Ok(());
    };
    let Ok(handle) = ctx.primary_image_handle() else {
        return Ok(());
    };
    // libheif applies the container's rotation and mirroring when decoding, and reports the size after them;
    // the EXIF orientation inside a HEIF is not to be applied again.
    f.width = Some(handle.width());
    f.height = Some(handle.height());
    f.orientation = Some(1);
    let mut ids = [0u32; 4];
    let n = handle.metadata_block_ids(&mut ids, b"Exif");
    if let Some(id) = ids[..n].first()
        && let Ok(m) = handle.metadata(*id)
    {
        // A 4-byte offset to the TIFF header, then the payload.
        if m.len() > 4 {
            let off = u32::from_be_bytes([m[0], m[1], m[2], m[3]]) as usize + 4;
            if let Some(x) = m.get(off..).and_then(tiff::read_exif) {
                let o = f.orientation;
                apply_exif(f, &x);
                f.orientation = o;
            }
        }
    }
    f.previewable = true;
    Ok(())
}

#[cfg(not(feature = "heic"))]
fn heif_facts(_: &mut Facts, _: &(impl ReadAt + ?Sized)) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_times() {
        assert_eq!(
            exif_time("2026:10:12 09:14:03", Some("+01:00")).unwrap(),
            ("2026-10-12T09:14:03+01:00".into(), 1_791_792_843)
        );
        assert_eq!(
            exif_time("2026:10:12 09:14:03", None).unwrap().0,
            "2026-10-12T09:14:03"
        );
        assert!(exif_time("0000:00:00 00:00:00", None).is_none());
    }
}
