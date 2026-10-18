//! The largest embedded JPEG preview of a camera RAW, found without a RAW decoder (a port of lith's
//! `core/raw/embedded-jpeg.js`).
//!
//! Where the preview is:
//!   RAF           the header points at it (offset/length at 84/88, big-endian)
//!   TIFF-shaped   CR2, NEF, ARW, DNG, PEF, SRW, ORF, RW2: JPEGInterchangeFormat (513/514), single JPEG strips
//!                 (259 = 6/7, 273/279) and Panasonic's JpgFromRaw (0x2e) in IFD0, its chain and SubIFDs
//!   anything else (CR3, makernote previews, a TIFF path that found nothing): a scan of the head for SOI markers

use super::jpeg;
use super::tiff::{Tiff, find};

pub const HEAD: usize = 1 << 20; // the IFDs and the RAF header are all within the first MB
pub const SCAN: usize = 8 << 20; // CR3 PRVW and makernote previews: within the first few MB
const PROBE: usize = 1 << 18; // a JPEG's markers up to SOS (Fuji puts ~78 kB of APP1s before SOF)
const MAX_IFDS: usize = 16;
const MIN_EDGE: u32 = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Embedded {
    pub offset: u64,
    pub length: u64,
    pub width: u32,
    pub height: u32,
    /// The EXIF orientation still to apply to the decoded pixels (1 = none).
    pub orientation: u8,
}

/// Reads bytes `[start, end)` of the file (clamped to its size).
pub trait ReadAt {
    fn read_at(&self, start: u64, end: u64) -> std::io::Result<Vec<u8>>;
    fn size(&self) -> u64;
}

impl ReadAt for std::fs::File {
    fn read_at(&self, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let end = end.min(self.size());
        if start >= end {
            return Ok(Vec::new());
        }
        let mut f = self;
        f.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0; (end - start) as usize];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }
    fn size(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }
}

impl ReadAt for [u8] {
    fn read_at(&self, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
        let (s, e) = (
            (start as usize).min(self.len()),
            (end as usize).min(self.len()),
        );
        Ok(self[s..e].to_vec())
    }
    fn size(&self) -> u64 {
        self.len() as u64
    }
}

struct Candidate {
    offset: u64,
    length: u64,
    info: jpeg::JpegInfo,
}

/// Candidate spans and the RAW's own orientation from a TIFF-shaped head.
fn tiff_candidates(head: &[u8]) -> Option<(Vec<(u64, u64)>, u8)> {
    let t = Tiff::open(head)?;
    let mut out = Vec::new();
    let mut orientation = 0u8;
    let mut seen = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::from([t.first]);
    while let Some(off) = queue.pop_front() {
        if off == 0 || seen.len() >= MAX_IFDS || !seen.insert(off) {
            continue;
        }
        let Some(ifd) = t.ifd(off) else { continue };
        if seen.len() == 1
            && let Some(o) = find(&ifd, 274)
                .and_then(|e| t.uint(e))
                .filter(|o| (1..=8).contains(o))
        {
            orientation = o as u8;
        }
        let one = |tag| find(&ifd, tag).and_then(|e| t.uint(e)).unwrap_or(0);
        if one(513) > 0 && one(514) > 0 {
            out.push((one(513) as u64, one(514) as u64));
        }
        if matches!(one(259), 6 | 7)
            && let (Some(so), Some(sc)) = (
                find(&ifd, 273).and_then(|e| t.uint(e)),
                find(&ifd, 279).and_then(|e| t.uint(e)),
            )
        {
            out.push((so as u64, sc as u64));
        }
        if let Some(e) = find(&ifd, 0x2e)
            && e.typ == 7
            && e.count > 4
            && let Some(o) = t.u32(e.at)
        {
            out.push((o as u64, e.count as u64));
        }
        if let Some(subs) = find(&ifd, 330).and_then(|e| t.uints(e)) {
            queue.extend(subs);
        }
        queue.push_back(t.next_ifd(off));
    }
    Some((out, orientation))
}

/// SOI positions that start a usable JPEG, each ending at the next EOI.
fn scan_candidates(b: &[u8]) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 < b.len() {
        if !(b[i] == 0xFF && b[i + 1] == 0xD8 && b[i + 2] == 0xFF) {
            i += 1;
            continue;
        }
        let Some(info) = jpeg::info(&b[i..(i + PROBE).min(b.len())]) else {
            i += 1;
            continue;
        };
        let Some(end) = b[i + 4..]
            .windows(2)
            .position(|w| w == [0xFF, 0xD9])
            .map(|p| i + 4 + p + 2)
        else {
            break; // runs past what was read
        };
        out.push(Candidate {
            offset: i as u64,
            length: (end - i) as u64,
            info,
        });
        i = end;
    }
    out
}

fn tiff_orientation(b: &[u8]) -> u8 {
    super::tiff::read_exif(b)
        .and_then(|x| x.orientation)
        .unwrap_or(0)
}

/// Finds the largest decodable embedded JPEG. Never fails on a malformed file; `Ok(None)` when there is none.
pub fn find_embedded(f: &(impl ReadAt + ?Sized)) -> std::io::Result<Option<Embedded>> {
    Ok(collect(f, false)?.into_iter().next())
}

/// Every embedded JPEG big enough for a preview, largest first, the file's structure AND a scan of its start
/// both searched: what to try when the first choice turns out not to decode (some cameras' maker notes point
/// at a damaged or differently encoded one).
pub fn all_embedded(f: &(impl ReadAt + ?Sized)) -> std::io::Result<Vec<Embedded>> {
    collect(f, true)
}

fn collect(f: &(impl ReadAt + ?Sized), scan_always: bool) -> std::io::Result<Vec<Embedded>> {
    let size = f.size();
    let head = f.read_at(0, HEAD as u64)?;
    let mut spans = Vec::new();
    let mut orientation = 0u8;
    if head.starts_with(b"FUJIFILMCCD-RAW") && head.len() >= 92 {
        let be = |at: usize| {
            u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]) as u64
        };
        spans.push((be(84), be(88)));
    } else if let Some((s, o)) = tiff_candidates(&head) {
        spans = s;
        orientation = o;
    }

    let mut found: Vec<Candidate> = Vec::new();
    fn consider(found: &mut Vec<Candidate>, c: Candidate) {
        if c.info.width.max(c.info.height) < MIN_EDGE || found.iter().any(|f| f.offset == c.offset)
        {
            return;
        }
        found.push(c);
    }
    for (offset, length) in spans {
        if !(offset > 0 && length > 4 && offset.saturating_add(length) <= size) {
            continue;
        }
        let probe = if offset as usize + PROBE <= head.len() {
            head[offset as usize..offset as usize + PROBE].to_vec()
        } else {
            f.read_at(offset, offset + length.min(PROBE as u64))?
        };
        if let Some(info) = jpeg::info(&probe) {
            consider(
                &mut found,
                Candidate {
                    offset,
                    length,
                    info,
                },
            );
        }
    }
    if found.is_empty() || scan_always {
        let bytes = if size as usize <= head.len() {
            head.clone()
        } else {
            f.read_at(0, SCAN as u64)?
        };
        for c in scan_candidates(&bytes) {
            consider(&mut found, c);
        }
        // CR3 keeps the orientation in its CMT1 box (a TIFF IFD0).
        if orientation == 0 {
            let limit = bytes.len().min(HEAD);
            if let Some(i) = bytes[..limit].windows(4).position(|w| w == b"CMT1") {
                orientation = tiff_orientation(&bytes[i + 4..(i + 4 + 65536).min(bytes.len())]);
            }
        }
    }
    let area = |c: &Candidate| c.info.width as u64 * c.info.height as u64;
    found.sort_by_key(|c| std::cmp::Reverse(area(c)));
    Ok(found
        .into_iter()
        .map(|b| Embedded {
            offset: b.offset,
            length: b.length,
            width: b.info.width,
            height: b.info.height,
            orientation: if b.info.orientation != 0 {
                b.info.orientation
            } else {
                orientation.max(1)
            },
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xC0, 0, 11, 8];
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&[1, 1, 0x11, 0, 0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9]);
        b
    }

    #[test]
    fn scan_finds_largest() {
        let mut file = b"junk".to_vec();
        file.extend(jpeg(160, 120));
        file.extend(b"more junk");
        let big_at = file.len();
        file.extend(jpeg(1620, 1080));
        let e = find_embedded(&file[..]).unwrap().unwrap();
        assert_eq!((e.width, e.height, e.offset), (1620, 1080, big_at as u64));
        assert_eq!(e.orientation, 1);
    }

    #[test]
    fn nothing_found() {
        assert_eq!(find_embedded(&b"not a raw at all"[..]).unwrap(), None);
    }
}
