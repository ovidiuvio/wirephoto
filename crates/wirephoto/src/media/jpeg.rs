//! JPEG structure: size, EXIF and ICC profile from the markers, without decoding.

/// Size and EXIF orientation of a JPEG from its markers up to SOS; `None` for anything not decodable as a
/// baseline/progressive JPEG (lossless, hierarchical and arithmetic-coded frames are refused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegInfo {
    pub width: u32,
    pub height: u32,
    /// 0 when the JPEG carries no EXIF orientation.
    pub orientation: u8,
}

/// Calls `f(marker, segment)` for each marker segment before SOS. Stops early when `f` returns false.
fn segments<'a>(b: &'a [u8], mut f: impl FnMut(u8, &'a [u8]) -> bool) -> bool {
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return false;
    }
    let mut at = 2;
    while at + 4 <= b.len() {
        if b[at] != 0xFF {
            return false;
        }
        let m = b[at + 1];
        if m == 0xFF {
            at += 1;
            continue;
        }
        if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
            at += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[at + 2], b[at + 3]]) as usize;
        if len < 2 {
            return false;
        }
        let seg = &b[(at + 4).min(b.len())..(at + 2 + len).min(b.len())];
        if !f(m, seg) || m == 0xDA {
            return true;
        }
        at += 2 + len;
    }
    true
}

pub fn info(b: &[u8]) -> Option<JpegInfo> {
    let (mut width, mut height, mut orientation, mut ok) = (0u32, 0u32, 0u8, true);
    let parsed = segments(b, |m, seg| {
        match m {
            0xC0..=0xC2 => {
                if seg.len() >= 5 {
                    height = u16::from_be_bytes([seg[1], seg[2]]) as u32;
                    width = u16::from_be_bytes([seg[3], seg[4]]) as u32;
                }
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                ok = false;
                return false;
            }
            0xE1 if seg.starts_with(b"Exif\0\0") => {
                if let Some(o) = super::tiff::read_exif(&seg[6..]).and_then(|x| x.orientation) {
                    orientation = o;
                }
            }
            _ => {}
        }
        true
    });
    (parsed && ok && width > 0 && height > 0).then_some(JpegInfo {
        width,
        height,
        orientation,
    })
}

/// A baseline JPEG (SOF0) in YCbCr or grey: what GPU decoders take. Progressive, arithmetic-coded and CMYK
/// ones are not.
pub fn baseline(b: &[u8]) -> bool {
    let mut sof0 = None;
    segments(b, |m, seg| match m {
        0xC0 => {
            sof0 = seg.get(5).copied();
            false
        }
        0xC1..=0xCF if m != 0xC4 && m != 0xC8 && m != 0xCC => false,
        _ => true,
    });
    matches!(sof0, Some(1 | 3))
}

/// The JPEG's EXIF payload (the TIFF structure after `Exif\0\0`).
pub fn exif(b: &[u8]) -> Option<&[u8]> {
    let mut out = None;
    segments(b, |m, seg| {
        if m == 0xE1 && seg.starts_with(b"Exif\0\0") {
            out = Some(&seg[6..]);
            return false;
        }
        true
    });
    out
}

/// The ICC profile, reassembled from its APP2 chunks.
pub fn icc(b: &[u8]) -> Option<Vec<u8>> {
    let mut chunks: Vec<(u8, &[u8])> = Vec::new();
    segments(b, |m, seg| {
        if m == 0xE2 && seg.len() > 14 && seg.starts_with(b"ICC_PROFILE\0") {
            chunks.push((seg[12], &seg[14..]));
        }
        true
    });
    if chunks.is_empty() {
        return None;
    }
    chunks.sort_by_key(|c| c.0);
    Some(
        chunks
            .into_iter()
            .flat_map(|c| c.1.iter().copied())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn tiny_jpeg_header(w: u16, h: u16, exif: Option<&[u8]>) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8];
        if let Some(x) = exif {
            b.extend_from_slice(&[0xFF, 0xE1]);
            b.extend_from_slice(&((x.len() + 8) as u16).to_be_bytes());
            b.extend_from_slice(b"Exif\0\0");
            b.extend_from_slice(x);
        }
        b.extend_from_slice(&[0xFF, 0xC0, 0, 11, 8]);
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&[1, 1, 0x11, 0]);
        b.extend_from_slice(&[0xFF, 0xDA, 0, 2]);
        b
    }

    #[test]
    fn reads_size_and_orientation() {
        let x = crate::media::tiff::tests::sample_exif(8, "2026:01:01 00:00:00", "+00:00");
        let b = tiny_jpeg_header(640, 480, Some(&x));
        assert_eq!(
            info(&b),
            Some(JpegInfo {
                width: 640,
                height: 480,
                orientation: 8
            })
        );
        assert!(exif(&b).is_some());
        assert_eq!(
            info(&tiny_jpeg_header(10, 20, None)).unwrap().orientation,
            0
        );
    }

    #[test]
    fn refuses_lossless() {
        let mut b = tiny_jpeg_header(10, 10, None);
        b[3] = 0xC3;
        assert_eq!(info(&b), None);
    }
}
