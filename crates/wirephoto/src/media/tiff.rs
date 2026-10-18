//! A bounded TIFF directory reader (after lith's `core/raw/tiff.js`): reads only the directories and tags it is
//! asked for, and checks every count and offset against the buffer, so a truncated or hostile file gives `None`,
//! never a panic, an out-of-range read or a loop.

const MAX_ENTRIES: usize = 1000;
const MAX_VALUE_BYTES: usize = 1 << 20;

fn type_size(t: u16) -> usize {
    match t {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct IfdEntry {
    pub tag: u16,
    pub typ: u16,
    pub count: u32,
    /// Offset of the 4-byte value/offset field, relative to the TIFF start.
    pub at: usize,
}

pub struct Tiff<'a> {
    b: &'a [u8],
    pub little: bool,
    pub first: u32,
}

impl<'a> Tiff<'a> {
    /// A TIFF header at the start of `b` (including Olympus ORF and Panasonic RW2 magic).
    pub fn open(b: &'a [u8]) -> Option<Tiff<'a>> {
        if b.len() < 8 {
            return None;
        }
        let little = match &b[..4] {
            b"II*\0" | b"IIRO" | b"IIRS" | b"IIU\0" => true,
            b"MM\0*" => false,
            _ => return None,
        };
        let t = Tiff {
            b,
            little,
            first: 0,
        };
        let first = t.u32(4)?;
        Some(Tiff { first, ..t })
    }

    pub fn u16(&self, at: usize) -> Option<u16> {
        let s = self.b.get(at..at + 2)?;
        Some(if self.little {
            u16::from_le_bytes([s[0], s[1]])
        } else {
            u16::from_be_bytes([s[0], s[1]])
        })
    }

    pub fn u32(&self, at: usize) -> Option<u32> {
        let s = self.b.get(at..at + 4)?;
        let a = [s[0], s[1], s[2], s[3]];
        Some(if self.little {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        })
    }

    /// The entries of the IFD at `offset`.
    pub fn ifd(&self, offset: u32) -> Option<Vec<IfdEntry>> {
        let offset = offset as usize;
        let n = self.u16(offset)? as usize;
        if n == 0 || n > MAX_ENTRIES || offset + 2 + n * 12 > self.b.len() {
            return None;
        }
        (0..n)
            .map(|i| {
                let o = offset + 2 + i * 12;
                Some(IfdEntry {
                    tag: self.u16(o)?,
                    typ: self.u16(o + 2)?,
                    count: self.u32(o + 4)?,
                    at: o + 8,
                })
            })
            .collect()
    }

    /// The offset of the IFD after the one at `offset` (0 when none).
    pub fn next_ifd(&self, offset: u32) -> u32 {
        let offset = offset as usize;
        self.u16(offset)
            .and_then(|n| self.u32(offset + 2 + n as usize * 12))
            .unwrap_or(0)
    }

    /// Where an entry's value bytes are: (start, length).
    fn span(&self, e: &IfdEntry) -> Option<(usize, usize)> {
        let n = type_size(e.typ).checked_mul(e.count as usize)?;
        if n == 0 || n > MAX_VALUE_BYTES {
            return None;
        }
        if n <= 4 {
            return Some((e.at, n));
        }
        let at = self.u32(e.at)? as usize;
        (at.checked_add(n)? <= self.b.len()).then_some((at, n))
    }

    /// Integer values of a SHORT/LONG entry (rationals are not needed here).
    pub fn uints(&self, e: &IfdEntry) -> Option<Vec<u32>> {
        let (at, _) = self.span(e)?;
        (0..e.count as usize)
            .map(|i| match e.typ {
                3 | 8 => self.u16(at + i * 2).map(u32::from),
                4 | 9 | 13 => self.u32(at + i * 4),
                _ => None,
            })
            .collect()
    }

    pub fn uint(&self, e: &IfdEntry) -> Option<u32> {
        match self.uints(e)?.as_slice() {
            [v] => Some(*v),
            _ => None,
        }
    }

    /// An ASCII entry, trimmed of NULs and spaces.
    pub fn ascii(&self, e: &IfdEntry) -> Option<String> {
        if e.typ != 2 {
            return None;
        }
        let (at, n) = self.span(e)?;
        let s = std::str::from_utf8(&self.b[at..at + n]).ok()?;
        let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        (!s.is_empty()).then(|| s.to_string())
    }
}

pub fn find(ifd: &[IfdEntry], tag: u16) -> Option<&IfdEntry> {
    ifd.iter().find(|e| e.tag == tag)
}

/// What EXIF says about a photo: orientation, capture time (with its offset when recorded) and the pixel size.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Exif {
    pub orientation: Option<u8>,
    pub taken: Option<String>,
    pub offset: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Reads EXIF from a TIFF structure (an `Exif\0\0` payload, a TIFF file or a TIFF-based RAW).
pub fn read_exif(b: &[u8]) -> Option<Exif> {
    let t = Tiff::open(b)?;
    let ifd0 = t.ifd(t.first)?;
    let mut x = Exif::default();
    if let Some(o) = find(&ifd0, 274).and_then(|e| t.uint(e))
        && (1..=8).contains(&o)
    {
        x.orientation = Some(o as u8);
    }
    let mut datetime = find(&ifd0, 306).and_then(|e| t.ascii(e));
    if let Some(exif_ifd) = find(&ifd0, 34665)
        .and_then(|e| t.uint(e))
        .and_then(|o| t.ifd(o))
    {
        if let Some(d) = find(&exif_ifd, 36867).and_then(|e| t.ascii(e)) {
            datetime = Some(d);
        }
        x.offset = find(&exif_ifd, 36881).and_then(|e| t.ascii(e));
        x.width = find(&exif_ifd, 40962)
            .and_then(|e| t.uint(e))
            .filter(|v| *v > 0);
        x.height = find(&exif_ifd, 40963)
            .and_then(|e| t.uint(e))
            .filter(|v| *v > 0);
    }
    // CR3 keeps its Exif IFD as a TIFF of its own (the CMT2 box): the tags sit in its IFD0.
    if let Some(d) = find(&ifd0, 36867).and_then(|e| t.ascii(e)) {
        datetime = Some(d);
        x.offset = x
            .offset
            .or_else(|| find(&ifd0, 36881).and_then(|e| t.ascii(e)));
    }
    x.taken = datetime;
    Some(x)
}

/// The largest image size declared by any directory of a TIFF-based file (IFD0, its chain, SubIFDs): for a RAW,
/// the sensor's image.
pub fn largest_ifd_size(b: &[u8]) -> Option<(u32, u32)> {
    let t = Tiff::open(b)?;
    let mut best: Option<(u32, u32)> = None;
    let mut queue = vec![t.first];
    let mut seen = std::collections::HashSet::new();
    while let Some(off) = queue.pop() {
        if off == 0 || !seen.insert(off) || seen.len() > 16 {
            continue;
        }
        let Some(ifd) = t.ifd(off) else { continue };
        let w = find(&ifd, 256).and_then(|e| t.uint(e));
        let h = find(&ifd, 257).and_then(|e| t.uint(e));
        if let (Some(w), Some(h)) = (w, h)
            && w > 0
            && h > 0
            && best.is_none_or(|(bw, bh)| w as u64 * h as u64 > bw as u64 * bh as u64)
        {
            best = Some((w, h));
        }
        if let Some(subs) = find(&ifd, 330).and_then(|e| t.uints(e)) {
            queue.extend(subs);
        }
        queue.push(t.next_ifd(off));
    }
    best
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A little-endian TIFF with IFD0 {orientation, Exif IFD -> {DateTimeOriginal, OffsetTimeOriginal}}.
    pub fn sample_exif(orientation: u16, taken: &str, offset: &str) -> Vec<u8> {
        let mut b = b"II*\0".to_vec();
        b.extend_from_slice(&8u32.to_le_bytes());
        // IFD0 at 8: 2 entries
        let ifd0 = 8usize;
        let exif_ifd = ifd0 + 2 + 2 * 12 + 4;
        let strings = exif_ifd + 2 + 2 * 12 + 4;
        let taken_z = format!("{taken}\0");
        let offset_z = format!("{offset}\0");
        b.extend_from_slice(&2u16.to_le_bytes());
        entry(&mut b, 274, 3, 1, orientation as u32);
        entry(&mut b, 34665, 4, 1, exif_ifd as u32);
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        entry(&mut b, 36867, 2, taken_z.len() as u32, strings as u32);
        entry(
            &mut b,
            36881,
            2,
            offset_z.len() as u32,
            (strings + taken_z.len()) as u32,
        );
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(taken_z.as_bytes());
        b.extend_from_slice(offset_z.as_bytes());
        b
    }

    fn entry(b: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) {
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&typ.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        if typ == 3 && count == 1 {
            b.extend_from_slice(&(value as u16).to_le_bytes());
            b.extend_from_slice(&[0, 0]);
        } else {
            b.extend_from_slice(&value.to_le_bytes());
        }
    }

    #[test]
    fn reads_exif() {
        let b = sample_exif(6, "2026:10:12 09:14:03", "+01:00");
        let x = read_exif(&b).unwrap();
        assert_eq!(x.orientation, Some(6));
        assert_eq!(x.taken.as_deref(), Some("2026:10:12 09:14:03"));
        assert_eq!(x.offset.as_deref(), Some("+01:00"));
    }

    #[test]
    fn hostile_input_is_none() {
        assert!(read_exif(b"II*\0\xff\xff\xff\xff").is_none());
        let mut b = sample_exif(1, "x", "y");
        b.truncate(20);
        assert!(read_exif(&b).is_none());
    }
}
