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

/// The primary image's transformations, from the start of a HEIF file (its `meta` box comes first).
pub fn transforms(file_head: &[u8]) -> Option<Vec<Transform>> {
    let meta = child(file_head, b"meta")?;
    let meta = meta.get(4..)?; // a full box: version and flags
    let pitm = child(meta, b"pitm")?;
    let primary = match pitm.first()? {
        0 => u16::from_be_bytes(pitm.get(4..6)?.try_into().ok()?) as u32,
        _ => u32::from_be_bytes(pitm.get(4..8)?.try_into().ok()?),
    };
    let iprp = child(meta, b"iprp")?;
    let props: Vec<([u8; 4], &[u8])> = boxes(child(iprp, b"ipco")?).collect();
    let mut out = Vec::new();
    for (typ, ipma) in boxes(iprp) {
        if &typ != b"ipma" || ipma.len() < 8 {
            continue;
        }
        let (version, flags) = (ipma[0], ipma[3]);
        let count = u32::from_be_bytes(ipma[4..8].try_into().ok()?);
        let mut at = 8usize;
        for _ in 0..count {
            let item = if version < 1 {
                let v = u16::from_be_bytes(ipma.get(at..at + 2)?.try_into().ok()?) as u32;
                at += 2;
                v
            } else {
                let v = u32::from_be_bytes(ipma.get(at..at + 4)?.try_into().ok()?);
                at += 4;
                v
            };
            let n = *ipma.get(at)? as usize;
            at += 1;
            for _ in 0..n {
                let index = if flags & 1 != 0 {
                    let v = u16::from_be_bytes(ipma.get(at..at + 2)?.try_into().ok()?) & 0x7fff;
                    at += 2;
                    v as usize
                } else {
                    let v = ipma.get(at)? & 0x7f;
                    at += 1;
                    v as usize
                };
                if item != primary || index == 0 {
                    continue;
                }
                match props.get(index - 1) {
                    Some((t, p)) if t == b"irot" => out.push(Transform::Rotate(p.first()? & 3)),
                    Some((t, p)) if t == b"imir" => {
                        out.push(Transform::Mirror(p.first()? & 1 == 1))
                    }
                    _ => {}
                }
            }
        }
    }
    Some(out)
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
    }
}
