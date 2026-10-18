//! What a file is: its kind and MIME type, from its content (with the extension as a tie-breaker for RAW
//! formats, most of which are TIFF containers).

use wirephoto_api::Kind;

pub fn to_i64(k: Kind) -> i64 {
    match k {
        Kind::Folder => 0,
        Kind::Image => 1,
        Kind::Raw => 2,
        Kind::Video => 3,
        Kind::Other => 4,
    }
}

pub fn from_i64(v: i64) -> Kind {
    match v {
        0 => Kind::Folder,
        1 => Kind::Image,
        2 => Kind::Raw,
        3 => Kind::Video,
        _ => Kind::Other,
    }
}

pub fn parse(s: &str) -> Option<Kind> {
    Some(match s {
        "folder" => Kind::Folder,
        "image" => Kind::Image,
        "raw" => Kind::Raw,
        "video" => Kind::Video,
        "other" => Kind::Other,
        _ => return None,
    })
}

/// RAW extensions and their MIME types.
const RAW: &[(&str, &str)] = &[
    ("3fr", "image/x-hasselblad-3fr"),
    ("ari", "image/x-arri-ari"),
    ("arw", "image/x-sony-arw"),
    ("cr2", "image/x-canon-cr2"),
    ("cr3", "image/x-canon-cr3"),
    ("crw", "image/x-canon-crw"),
    ("dcr", "image/x-kodak-dcr"),
    ("dng", "image/x-adobe-dng"),
    ("erf", "image/x-epson-erf"),
    ("fff", "image/x-hasselblad-fff"),
    ("iiq", "image/x-phaseone-iiq"),
    ("k25", "image/x-kodak-k25"),
    ("kdc", "image/x-kodak-kdc"),
    ("mef", "image/x-mamiya-mef"),
    ("mos", "image/x-leaf-mos"),
    ("mrw", "image/x-minolta-mrw"),
    ("nef", "image/x-nikon-nef"),
    ("nrw", "image/x-nikon-nrw"),
    ("orf", "image/x-olympus-orf"),
    ("ori", "image/x-olympus-orf"),
    ("pef", "image/x-pentax-pef"),
    ("raf", "image/x-fuji-raf"),
    ("raw", "image/x-panasonic-raw"),
    ("rw2", "image/x-panasonic-rw2"),
    ("rwl", "image/x-leica-rwl"),
    ("sr2", "image/x-sony-sr2"),
    ("srf", "image/x-sony-srf"),
    ("srw", "image/x-samsung-srw"),
    ("x3f", "image/x-sigma-x3f"),
];

const IMAGE: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("jpe", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("hif", "image/heif"),
    ("jxl", "image/jxl"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("bmp", "image/bmp"),
    ("psd", "image/vnd.adobe.photoshop"),
    ("psb", "image/vnd.adobe.photoshop"),
];

const VIDEO: &[(&str, &str)] = &[
    ("mp4", "video/mp4"),
    ("m4v", "video/x-m4v"),
    ("mov", "video/quicktime"),
    ("mkv", "video/x-matroska"),
    ("webm", "video/webm"),
    ("avi", "video/x-msvideo"),
    ("mts", "video/mp2t"),
    ("m2ts", "video/mp2t"),
    ("3gp", "video/3gpp"),
    ("mpg", "video/mpeg"),
    ("mpeg", "video/mpeg"),
];

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// A first guess from the name alone, used until the content has been read.
pub fn guess(name: &str) -> (Kind, Option<&'static str>) {
    let ext = ext_of(name);
    for (table, kind) in [(RAW, Kind::Raw), (IMAGE, Kind::Image), (VIDEO, Kind::Video)] {
        if let Some((_, mime)) = table.iter().find(|(e, _)| *e == ext) {
            return (kind, Some(mime));
        }
    }
    (Kind::Other, None)
}

/// The kind and MIME type from the first bytes of the file (a few kB is enough).
pub fn sniff(name: &str, head: &[u8]) -> (Kind, Option<String>) {
    let ext = ext_of(name);
    let raw_by_ext = RAW.iter().find(|(e, _)| *e == ext);
    let tiff = head.starts_with(b"II*\0") || head.starts_with(b"MM\0*");
    // RAW formats: TIFF containers and their relatives, told apart by extension; a few have their own magic.
    if let Some((_, mime)) = raw_by_ext {
        let own_magic = head.starts_with(b"FUJIFILMCCD-RAW")
            || head.starts_with(b"IIRO")
            || head.starts_with(b"IIRS")
            || head.starts_with(b"IIU\0")
            || head.starts_with(b"\0MRM")
            || head.starts_with(b"FOVb")
            || (head.len() >= 12 && &head[4..12] == b"ftypcrx ");
        if tiff || own_magic {
            return (Kind::Raw, Some((*mime).into()));
        }
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let brand = &head[8..12];
        let heif = [
            &b"heic"[..],
            b"heix",
            b"heim",
            b"heis",
            b"hevc",
            b"hevx",
            b"mif1",
            b"msf1",
        ];
        if brand == b"avif" || brand == b"avis" {
            return (Kind::Image, Some("image/avif".into()));
        }
        if heif.contains(&brand) {
            return (
                Kind::Image,
                Some(
                    if brand == b"mif1" || brand == b"msf1" {
                        "image/heif"
                    } else {
                        "image/heic"
                    }
                    .into(),
                ),
            );
        }
    }
    if tiff {
        return (Kind::Image, Some("image/tiff".into()));
    }
    if let Some(t) = infer::get(head) {
        let mime = t.mime_type();
        let kind = match t.matcher_type() {
            infer::MatcherType::Image => Kind::Image,
            infer::MatcherType::Video => Kind::Video,
            _ => Kind::Other,
        };
        // infer calls some RAWs "image/x-canon-cr2" etc. and a bare TIFF "image/tiff"; trust the extension
        // table for those, already handled above.
        return (kind, Some(mime.into()));
    }
    if head.starts_with(b"8BPS") {
        return (Kind::Image, Some("image/vnd.adobe.photoshop".into()));
    }
    if head.starts_with(&[0xFF, 0x0A]) || head.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n") {
        return (Kind::Image, Some("image/jxl".into()));
    }
    let (kind, mime) = guess(name);
    match kind {
        // A name that says image or video but content that does not: not something we can treat as media.
        Kind::Image | Kind::Raw | Kind::Video => (Kind::Other, None),
        _ => (kind, mime.map(Into::into)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses() {
        assert_eq!(guess("DSC01234.ARW").0, Kind::Raw);
        assert_eq!(guess("a.JPG").0, Kind::Image);
        assert_eq!(guess("clip.mov").0, Kind::Video);
        assert_eq!(guess("notes.txt").0, Kind::Other);
    }

    #[test]
    fn sniffs() {
        assert_eq!(
            sniff(
                "x.jpg",
                &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F', 0]
            )
            .0,
            Kind::Image
        );
        assert_eq!(sniff("x.arw", b"II*\0\x08\0\0\0").0, Kind::Raw);
        assert_eq!(
            sniff("x.tif", b"II*\0\x08\0\0\0").1.as_deref(),
            Some("image/tiff")
        );
        assert_eq!(sniff("x.jpg", b"hello world, not a jpeg").0, Kind::Other);
        let mut heic = vec![0, 0, 0, 0x18];
        heic.extend_from_slice(b"ftypheic\0\0\0\0");
        assert_eq!(sniff("IMG.HEIC", &heic).1.as_deref(), Some("image/heic"));
    }
}
