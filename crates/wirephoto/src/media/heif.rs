//! The rotation and mirroring a HEIF container gives its primary image (`irot`, `imir`; ISO/IEC 23008-12), read
//! from the boxes themselves: decoders that do not apply them (ffmpeg before 8) leave the picture as stored.

/// The boxes in `b`: (type, payload).
fn boxes(b: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        if at + 8 > b.len() {
            return None;
        }
        let size32 = u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        let typ: [u8; 4] = b[at + 4..at + 8].try_into().unwrap();
        let (head, size) = match size32 {
            0 => (8, b.len() - at),
            1 if at + 16 <= b.len() => (
                16,
                u64::from_be_bytes(b[at + 8..at + 16].try_into().unwrap()) as usize,
            ),
            1 => return None,
            n => (8, n),
        };
        if size < head || at + size > b.len() {
            // A box that runs past what was read: what of it there is.
            let payload = &b[(at + head).min(b.len())..];
            at = b.len();
            return Some((typ, payload));
        }
        let payload = &b[at + head..at + size];
        at += size;
        Some((typ, payload))
    })
}

fn child<'a>(b: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(b).find(|(t, _)| t == typ).map(|(_, p)| p)
}

/// A transformation of the primary image, in the order the container lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// Quarter turns anticlockwise.
    Rotate(u8),
    /// `imir`'s axis bit: `true` swaps left and right (what Apple writes for EXIF orientation 2, and libheif
    /// calls horizontal), `false` top and bottom.
    Mirror(bool),
}

/// Reads a big-endian unsigned integer of `n` bytes (0, 2, 4 or 8) at `*at`, moving past it.
fn uint(b: &[u8], at: &mut usize, n: usize) -> Option<u64> {
    let v = b
        .get(*at..*at + n)?
        .iter()
        .fold(0u64, |v, &x| v << 8 | x as u64);
    *at += n;
    Some(v)
}

/// What the `meta` box says about the items: enough to find the primary image, its properties, and the tiles
/// of a grid.
struct Meta<'a> {
    primary: u32,
    /// Item types (`infe`).
    types: std::collections::HashMap<u32, [u8; 4]>,
    /// Properties (`ipco`), and each item's, in order (`ipma`; 1-based indices).
    props: Vec<([u8; 4], &'a [u8])>,
    assoc: std::collections::HashMap<u32, Vec<usize>>,
    /// Where each item's bytes are (`iloc`): construction method (0 file, 1 `idat`), (offset, length) extents.
    locs: std::collections::HashMap<u32, (u8, Vec<(u64, u64)>)>,
    /// Derived-image references (`iref` `dimg`): from, to.
    dimg: std::collections::HashMap<u32, Vec<u32>>,
    idat: Option<&'a [u8]>,
}

fn meta(file: &[u8]) -> Option<Meta<'_>> {
    let meta = child(file, b"meta")?.get(4..)?; // a full box: version and flags
    let pitm = child(meta, b"pitm")?;
    let primary = match pitm.first()? {
        0 => u16::from_be_bytes(pitm.get(4..6)?.try_into().ok()?) as u32,
        _ => u32::from_be_bytes(pitm.get(4..8)?.try_into().ok()?),
    };
    let mut m = Meta {
        primary,
        types: Default::default(),
        props: Vec::new(),
        assoc: Default::default(),
        locs: Default::default(),
        dimg: Default::default(),
        idat: child(meta, b"idat"),
    };
    if let Some(iinf) = child(meta, b"iinf") {
        let skip = if iinf.first() == Some(&0) { 6 } else { 8 };
        for (t, infe) in boxes(iinf.get(skip..).unwrap_or(&[])) {
            if &t != b"infe" || infe.first().is_none_or(|v| *v < 2) {
                continue;
            }
            let mut at = 4;
            let id_len = if infe[0] == 2 { 2 } else { 4 };
            let (Some(id), Some(_)) = (uint(infe, &mut at, id_len), uint(infe, &mut at, 2)) else {
                continue;
            };
            if let Some(typ) = infe
                .get(at..at + 4)
                .and_then(|t| <[u8; 4]>::try_from(t).ok())
            {
                m.types.insert(id as u32, typ);
            }
        }
    }
    if let Some(iprp) = child(meta, b"iprp") {
        m.props = boxes(child(iprp, b"ipco").unwrap_or(&[])).collect();
        for (typ, ipma) in boxes(iprp) {
            if &typ != b"ipma" || ipma.len() < 8 {
                continue;
            }
            let (version, flags) = (ipma[0], ipma[3]);
            let mut at = 4;
            let count = uint(ipma, &mut at, 4)?;
            for _ in 0..count {
                let item = uint(ipma, &mut at, if version < 1 { 2 } else { 4 })? as u32;
                let n = uint(ipma, &mut at, 1)?;
                for _ in 0..n {
                    let index = if flags & 1 != 0 {
                        uint(ipma, &mut at, 2)? & 0x7fff
                    } else {
                        uint(ipma, &mut at, 1)? & 0x7f
                    };
                    m.assoc.entry(item).or_default().push(index as usize);
                }
            }
        }
    }
    if let Some(iloc) = child(meta, b"iloc") {
        let version = *iloc.first()?;
        let mut at = 4;
        let sizes = uint(iloc, &mut at, 2)?;
        let (off, len, base, idx) = (
            (sizes >> 12 & 15) as usize,
            (sizes >> 8 & 15) as usize,
            (sizes >> 4 & 15) as usize,
            if version >= 1 {
                (sizes & 15) as usize
            } else {
                0
            },
        );
        let count = uint(iloc, &mut at, if version < 2 { 2 } else { 4 })?;
        for _ in 0..count {
            let id = uint(iloc, &mut at, if version < 2 { 2 } else { 4 })? as u32;
            let method = if version >= 1 {
                (uint(iloc, &mut at, 2)? & 15) as u8
            } else {
                0
            };
            uint(iloc, &mut at, 2)?; // data reference
            let base_offset = uint(iloc, &mut at, base)?;
            let extents = uint(iloc, &mut at, 2)?;
            let mut v = Vec::new();
            for _ in 0..extents {
                uint(iloc, &mut at, idx)?;
                let o = uint(iloc, &mut at, off)?;
                let l = uint(iloc, &mut at, len)?;
                v.push((base_offset + o, l));
            }
            m.locs.insert(id, (method, v));
        }
    }
    if let Some(iref) = child(meta, b"iref") {
        let id_len = if iref.first() == Some(&0) { 2 } else { 4 };
        for (typ, r) in boxes(iref.get(4..).unwrap_or(&[])) {
            if &typ != b"dimg" {
                continue;
            }
            let mut at = 0;
            let (Some(from), Some(n)) = (uint(r, &mut at, id_len), uint(r, &mut at, 2)) else {
                continue;
            };
            let to: Vec<u32> = (0..n)
                .map_while(|_| uint(r, &mut at, id_len).map(|v| v as u32))
                .collect();
            m.dimg.insert(from as u32, to);
        }
    }
    Some(m)
}

impl Meta<'_> {
    /// An item's properties, in order.
    fn props_of(&self, item: u32) -> impl Iterator<Item = (usize, &([u8; 4], &[u8]))> {
        self.assoc
            .get(&item)
            .into_iter()
            .flatten()
            .filter_map(|&i| Some((i, self.props.get(i.checked_sub(1)?)?)))
    }

    /// An item's bytes.
    fn data(&self, file: &[u8], item: u32) -> Option<Vec<u8>> {
        let (method, extents) = self.locs.get(&item)?;
        let src = match method {
            0 => file,
            1 => self.idat?,
            _ => return None,
        };
        let mut out = Vec::new();
        for &(o, l) in extents {
            let (o, l) = (o as usize, l as usize);
            // A length of 0 means "to the end".
            let end = if l == 0 { src.len() } else { o.checked_add(l)? };
            out.extend_from_slice(src.get(o..end)?);
        }
        Some(out)
    }
}

/// The primary image's transformations, from the start of a HEIF file (its `meta` box comes first).
pub fn transforms(file_head: &[u8]) -> Option<Vec<Transform>> {
    let m = meta(file_head)?;
    let mut out = Vec::new();
    for (_, (typ, p)) in m.props_of(m.primary) {
        match typ {
            b"irot" => out.push(Transform::Rotate(p.first()? & 3)),
            b"imir" => out.push(Transform::Mirror(p.first()? & 1 == 1)),
            _ => {}
        }
    }
    Some(out)
}

/// A HEIC grid as one HEVC stream: its tiles, row by row, as the frames of an Annex B elementary stream, so a
/// single decoder (one GPU session) decodes them all.
#[derive(Debug)]
pub struct GridStream {
    pub rows: u32,
    pub cols: u32,
    /// The picture's size: the tiles' area cropped to it.
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    /// 4:2:0 (what GPU decoders take).
    pub yuv420: bool,
    pub stream: Vec<u8>,
}

/// The primary image's tiles as one stream, when it is a grid of HEVC tiles that share one configuration.
pub fn grid_stream(file: &[u8]) -> Option<GridStream> {
    let m = meta(file)?;
    if m.types.get(&m.primary) != Some(b"grid") {
        return None;
    }
    let grid = m.data(file, m.primary)?;
    let (rows, cols) = (*grid.get(2)? as u32 + 1, *grid.get(3)? as u32 + 1);
    let mut at = 4;
    let wide = grid.get(1)? & 1 == 1;
    let n = if wide { 4 } else { 2 };
    let (width, height) = (
        uint(&grid, &mut at, n)? as u32,
        uint(&grid, &mut at, n)? as u32,
    );
    let tiles = m.dimg.get(&m.primary)?;
    if tiles.len() as u32 != rows * cols {
        return None;
    }
    // One configuration for all: the same hvcC property.
    let config_of = |item: u32| {
        m.props_of(item)
            .find(|(_, (t, _))| t == b"hvcC")
            .map(|(i, (_, p))| (i, *p))
    };
    let (config, hvcc) = config_of(*tiles.first()?)?;
    if !tiles
        .iter()
        .all(|&t| m.types.get(&t) == Some(b"hvc1") && config_of(t).map(|c| c.0) == Some(config))
    {
        return None;
    }
    let chroma = hvcc.get(16)? & 3;
    let bit_depth = (hvcc.get(17)? & 7) + 8;
    let nal_len = (hvcc.get(21)? & 3) as usize + 1;
    let mut stream = Vec::new();
    let start = [0u8, 0, 0, 1];
    // The parameter sets (VPS, SPS, PPS), once.
    let arrays = *hvcc.get(22)?;
    let mut at = 23;
    for _ in 0..arrays {
        at += 1; // completeness and NAL unit type
        let count = uint(hvcc, &mut at, 2)?;
        for _ in 0..count {
            let len = uint(hvcc, &mut at, 2)? as usize;
            stream.extend_from_slice(&start);
            stream.extend_from_slice(hvcc.get(at..at + len)?);
            at += len;
        }
    }
    // Each tile: its NAL units, length-prefixed in the file, start-coded here.
    for &t in tiles {
        let data = m.data(file, t)?;
        let mut at = 0;
        while at < data.len() {
            let len = uint(&data, &mut at, nal_len)? as usize;
            stream.extend_from_slice(&start);
            stream.extend_from_slice(data.get(at..at + len)?);
            at += len;
        }
    }
    Some(GridStream {
        rows,
        cols,
        width,
        height,
        bit_depth,
        yuv420: chroma == 1,
        stream,
    })
}

/// The EXIF orientation (1..=8) the transformations amount to.
pub fn orientation(t: &[Transform]) -> u8 {
    // Where each corner of a 2x2 picture ends up, compared with what each EXIF orientation does to it.
    let mut g = [[0u8, 1], [2, 3]];
    for op in t {
        g = match op {
            Transform::Rotate(q) => {
                (0..*q).fold(g, |g, _| [[g[0][1], g[1][1]], [g[0][0], g[1][0]]])
            }
            Transform::Mirror(true) => [[g[0][1], g[0][0]], [g[1][1], g[1][0]]],
            Transform::Mirror(false) => [g[1], g[0]],
        };
    }
    (1..=8u8).find(|&o| exif(o) == g).unwrap_or(1)
}

/// What EXIF orientation `o` does to the 2x2 picture `[[0, 1], [2, 3]]` (as `preview::render::orient`).
fn exif(o: u8) -> [[u8; 2]; 2] {
    match o {
        2 => [[1, 0], [3, 2]],
        3 => [[3, 2], [1, 0]],
        4 => [[2, 3], [0, 1]],
        5 => [[0, 2], [1, 3]],
        6 => [[2, 0], [3, 1]],
        7 => [[3, 1], [2, 0]],
        8 => [[1, 3], [0, 2]],
        _ => [[0, 1], [2, 3]],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::render::{Rgb, orient};

    #[test]
    fn exif_table_matches_orient() {
        for o in 1..=8u8 {
            let img = Rgb {
                w: 2,
                h: 2,
                px: [0u8, 1, 2, 3].iter().flat_map(|&v| [v, 0, 0]).collect(),
            };
            let out = orient(img, o);
            let px: Vec<u8> = out.px.chunks(3).map(|p| p[0]).collect();
            assert_eq!(px, exif(o).concat(), "orientation {o}");
        }
    }

    #[test]
    fn transforms_compose() {
        assert_eq!(orientation(&[]), 1);
        assert_eq!(orientation(&[Transform::Rotate(1)]), 8); // a quarter turn anticlockwise
        assert_eq!(orientation(&[Transform::Rotate(3)]), 6);
        assert_eq!(orientation(&[Transform::Rotate(2)]), 3);
        assert_eq!(orientation(&[Transform::Mirror(true)]), 2);
        assert_eq!(orientation(&[Transform::Mirror(false)]), 4);
    }

    /// Apple's encoder, asked for each EXIF orientation, writes the boxes that mean it.
    #[test]
    fn apple_files() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heic");
        for o in 1..=8u8 {
            let b = std::fs::read(dir.join(format!("orientation-{o}.heic"))).unwrap();
            assert_eq!(
                orientation(&transforms(&b).unwrap()),
                o,
                "orientation-{o}.heic"
            );
        }
        let b = std::fs::read(dir.join("single-8bit.heic")).unwrap();
        assert_eq!(transforms(&b), Some(vec![]));
        assert!(grid_stream(&b).is_none(), "one picture, not a grid");
        let b = std::fs::read(dir.join("orientation-6.heic")).unwrap();
        let g = grid_stream(&b).unwrap();
        // 1100x734 stored landscape (shown portrait), in 512 px tiles.
        assert_eq!(
            (g.rows, g.cols, g.width, g.height, g.bit_depth, g.yuv420),
            (2, 3, 1100, 734, 8, true)
        );
        assert_eq!(&g.stream[..4], &[0, 0, 0, 1]);
    }
}
