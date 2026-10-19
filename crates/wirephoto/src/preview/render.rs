//! Making one preview: decode at (or near) the target size, composite transparency, resize, convert to sRGB,
//! apply orientation, encode a baseline JPEG without metadata (api.md section 5; design.md, Previews).

use anyhow::{Context, anyhow, bail};
use fast_image_resize as fr;
use turbojpeg::{Colorspace, Decompressor, PixelFormat, ScalingFactor, Subsamp};

use crate::media::{jpeg, rawjpeg};

/// 8-bit RGB pixels, tightly packed.
pub struct Rgb {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

/// A decoded source: pixels, the ICC profile they are in (None = sRGB), the orientation still to apply.
pub struct Decoded {
    pub img: Rgb,
    pub icc: Option<Vec<u8>>,
    pub orientation: u8,
}

/// Which encoder makes the JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    /// libjpeg-turbo: fastest, for sizes made on demand.
    Turbo,
    /// The cached size: jpegli, ~15% smaller at equal quality (design.md, Previews). Built without the `jpegli`
    /// feature, libjpeg-turbo with Huffman optimisation.
    Cache,
    /// 160 and 320 px made from the cached 640: libjpeg-turbo at a lower quality. Their source already carries
    /// the 640's compression, so a higher quality mostly spends bytes keeping it (measured on 50 camera photos
    /// against references made from the 2560 px preview, SSIMULACRA2 / KB at 320 px: q70 63.5 / 9.5, q80 65.6 /
    /// 11.6).
    Small,
}

const GREY: u8 = 128; // what transparent areas are composited onto

/// Decodes a JPEG, letting libjpeg-turbo's DCT scaling do most of the downscaling: the smallest scale whose
/// long edge still covers `target`.
pub fn decode_jpeg(bytes: &[u8], target: u32) -> anyhow::Result<Decoded> {
    let mut d = Decompressor::new()?;
    let header = d.read_header(bytes)?;
    let long = header.width.max(header.height);
    let factor = Decompressor::supported_scaling_factors()
        .into_iter()
        .filter(|f| f.num() <= f.denom()) // never upscale
        .filter(|f| f.scale(long) >= target as usize)
        .min_by(|a, b| (a.num() * b.denom()).cmp(&(b.num() * a.denom())))
        .unwrap_or(ScalingFactor::ONE);
    d.set_scaling_factor(factor)?;
    let scaled = header.scaled(factor);
    let (w, h) = (scaled.width, scaled.height);
    let info = jpeg::info(bytes);
    let orientation = info.map(|i| i.orientation).filter(|o| *o != 0).unwrap_or(1);
    let icc = jpeg::icc(bytes);
    let px = if matches!(header.colorspace, Colorspace::CMYK | Colorspace::YCCK) {
        let mut cmyk = vec![0u8; w * h * 4];
        d.decompress(
            bytes,
            turbojpeg::Image {
                pixels: &mut cmyk[..],
                width: w,
                pitch: w * 4,
                height: h,
                format: PixelFormat::CMYK,
            },
        )?;
        // Without a CMYK engine: the usual inverted (Adobe) CMYK to RGB. Previews only.
        let mut rgb = Vec::with_capacity(w * h * 3);
        for p in cmyk.as_chunks::<4>().0 {
            let k = p[3] as u32;
            rgb.extend([p[0], p[1], p[2]].map(|c| (c as u32 * k / 255) as u8));
        }
        return Ok(Decoded {
            img: Rgb {
                w: w as u32,
                h: h as u32,
                px: rgb,
            },
            icc: None,
            orientation,
        });
    } else {
        let mut px = vec![0u8; w * h * 3];
        d.decompress(
            bytes,
            turbojpeg::Image {
                pixels: &mut px[..],
                width: w,
                pitch: w * 3,
                height: h,
                format: PixelFormat::RGB,
            },
        )?;
        px
    };
    Ok(Decoded {
        img: Rgb {
            w: w as u32,
            h: h as u32,
            px,
        },
        icc,
        orientation,
    })
}

/// A RAW's embedded JPEG, decoded.
pub fn decode_raw(file: &(impl rawjpeg::ReadAt + ?Sized), target: u32) -> anyhow::Result<Decoded> {
    let first = rawjpeg::find_embedded(file)?.ok_or_else(|| anyhow!("no embedded preview"))?;
    let decode = |e: &rawjpeg::Embedded| -> anyhow::Result<Decoded> {
        let bytes = file.read_at(e.offset, e.offset + e.length)?;
        let mut d = decode_jpeg(&bytes, target)?;
        d.orientation = e.orientation;
        Ok(d)
    };
    match decode(&first) {
        Ok(d) => Ok(d),
        // The usual choice does not decode (an Olympus ORF's, for one): the other embedded images, largest first.
        Err(first_err) => rawjpeg::all_embedded(file)?
            .iter()
            .filter(|e| e.offset != first.offset)
            .find_map(|e| decode(e).ok())
            .ok_or(first_err),
    }
}

/// PNG, GIF (first frame), WebP, BMP, TIFF (first page) through the `image` crate.
pub fn decode_image(bytes: &[u8], mime: &str, orientation: u8) -> anyhow::Result<Decoded> {
    use image::ImageDecoder;
    let fmt = match mime {
        "image/png" => image::ImageFormat::Png,
        "image/gif" => image::ImageFormat::Gif,
        "image/webp" => image::ImageFormat::WebP,
        "image/bmp" => image::ImageFormat::Bmp,
        "image/tiff" => image::ImageFormat::Tiff,
        _ => bail!("cannot decode {mime}"),
    };
    let mut decoder =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), fmt).into_decoder()?;
    let icc = decoder.icc_profile().ok().flatten();
    let dynimg = image::DynamicImage::from_decoder(decoder)?;
    let img = if dynimg.color().has_alpha() {
        let rgba = dynimg.into_rgba8();
        let (w, h) = rgba.dimensions();
        Rgb {
            w,
            h,
            px: composite(rgba.as_raw()),
        }
    } else {
        let rgb = dynimg.into_rgb8();
        let (w, h) = rgb.dimensions();
        Rgb {
            w,
            h,
            px: rgb.into_raw(),
        }
    };
    Ok(Decoded {
        img,
        icc,
        orientation,
    })
}

#[cfg(feature = "heic")]
pub fn decode_heif(bytes: &[u8]) -> anyhow::Result<Decoded> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
    let ctx = HeifContext::read_from_bytes(bytes)?;
    let handle = ctx.primary_image_handle()?;
    let alpha = handle.has_alpha_channel();
    let lib = LibHeif::new();
    let chroma = if alpha {
        RgbChroma::Rgba
    } else {
        RgbChroma::Rgb
    };
    let image = lib.decode(&handle, ColorSpace::Rgb(chroma), None)?;
    let icc = handle.color_profile_raw().map(|p| p.data);
    let planes = image.planes();
    let plane = planes.interleaved.context("no interleaved plane")?;
    let (w, h, stride) = (plane.width, plane.height, plane.stride);
    let bpp = if alpha { 4 } else { 3 };
    let mut px = Vec::with_capacity(w as usize * h as usize * bpp);
    for row in 0..h as usize {
        px.extend_from_slice(&plane.data[row * stride..row * stride + w as usize * bpp]);
    }
    let px = if alpha { composite(&px) } else { px };
    // libheif has already applied the container's rotation and mirroring.
    Ok(Decoded {
        img: Rgb { w, h, px },
        icc,
        orientation: 1,
    })
}

/// The EXIF orientation of a HEIF's primary image (1 when it has no rotation or mirroring).
pub fn heif_orientation(bytes: &[u8]) -> u8 {
    crate::media::heif::transforms(bytes)
        .map(|t| crate::media::heif::orientation(&t))
        .unwrap_or(1)
}

/// What decoding a HEIF elsewhere needs from libheif: whether it has transparency, and its ICC profile.
pub struct HeifMeta {
    pub alpha: bool,
    pub icc: Option<Vec<u8>>,
    /// The EXIF orientation of the container's rotation and mirroring, read from its boxes: a decoder that
    /// does not apply them (ffmpeg before 8) leaves the picture as stored.
    pub orientation: u8,
}

/// The primary image's [`HeifMeta`] (the container is parsed, nothing decoded).
#[cfg(feature = "heic")]
pub fn heif_meta(bytes: &[u8]) -> Option<HeifMeta> {
    let ctx = libheif_rs::HeifContext::read_from_bytes(bytes).ok()?;
    let handle = ctx.primary_image_handle().ok()?;
    Some(HeifMeta {
        alpha: handle.has_alpha_channel(),
        icc: handle.color_profile_raw().map(|p| p.data),
        orientation: heif_orientation(bytes),
    })
}

#[cfg(not(feature = "heic"))]
pub fn heif_meta(bytes: &[u8]) -> Option<HeifMeta> {
    Some(HeifMeta {
        alpha: false,
        icc: None,
        orientation: heif_orientation(bytes),
    })
}

#[cfg(not(feature = "heic"))]
pub fn decode_heif(_: &[u8]) -> anyhow::Result<Decoded> {
    bail!("built without HEIF support")
}

/// RGBA onto neutral grey -> RGB.
fn composite(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for p in rgba.as_chunks::<4>().0 {
        let a = p[3] as u32;
        out.extend(
            [p[0], p[1], p[2]]
                .map(|c| ((c as u32 * a + GREY as u32 * (255 - a) + 127) / 255) as u8),
        );
    }
    out
}

/// Fits the image within `size` on its long edge (never enlarging).
pub fn resize(img: Rgb, size: u32) -> anyhow::Result<Rgb> {
    let long = img.w.max(img.h);
    if long <= size {
        return Ok(img);
    }
    let scale = size as f64 / long as f64;
    let (w, h) = (
        ((img.w as f64 * scale).round() as u32).max(1),
        ((img.h as f64 * scale).round() as u32).max(1),
    );
    let src = fr::images::Image::from_vec_u8(img.w, img.h, img.px, fr::PixelType::U8x3)?;
    let mut dst = fr::images::Image::new(w, h, fr::PixelType::U8x3);
    let opts =
        fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3));
    fr::Resizer::new().resize(&src, &mut dst, &opts)?;
    Ok(Rgb {
        w,
        h,
        px: dst.into_vec(),
    })
}

/// Converts pixels in `icc` to sRGB. Profiles that cannot be used are ignored (treated as sRGB).
pub fn to_srgb(img: &mut Rgb, icc: Option<&[u8]>) {
    let Some(icc) = icc else { return };
    let Ok(src) = moxcms::ColorProfile::new_from_slice(icc) else {
        return;
    };
    if src.color_space != moxcms::DataColorSpace::Rgb {
        return;
    }
    let dst = moxcms::ColorProfile::new_srgb();
    let Ok(t) = src.create_transform_8bit(
        moxcms::Layout::Rgb,
        &dst,
        moxcms::Layout::Rgb,
        Default::default(),
    ) else {
        return;
    };
    let mut out = vec![0u8; img.px.len()];
    if t.transform(&img.px, &mut out).is_ok() {
        img.px = out;
    }
}

/// Applies an EXIF orientation (1..=8) to the pixels.
pub fn orient(img: Rgb, orientation: u8) -> Rgb {
    if orientation <= 1 || orientation > 8 {
        return img;
    }
    let (w, h) = (img.w as usize, img.h as usize);
    let swap = orientation >= 5;
    let (ow, oh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u8; img.px.len()];
    for y in 0..h {
        for x in 0..w {
            // Where source pixel (x, y) lands.
            let (dx, dy) = match orientation {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (h - 1 - y, x),
                7 => (h - 1 - y, w - 1 - x),
                _ => (y, w - 1 - x), // 8
            };
            let s = (y * w + x) * 3;
            let d = (dy * ow + dx) * 3;
            out[d..d + 3].copy_from_slice(&img.px[s..s + 3]);
        }
    }
    Rgb {
        w: ow as u32,
        h: oh as u32,
        px: out,
    }
}

/// jpegli's quality for the cached size (cjpegli's `-q`): SSIMULACRA2 ~70 on camera photos at 640 px, measured
/// with scripts/bench-jpeg.py.
#[cfg(feature = "jpegli")]
const JPEGLI_QUALITY: i32 = 68;

/// Encodes a baseline 4:2:0 JPEG with no metadata.
pub fn encode(img: &Rgb, encoder: Encoder) -> anyhow::Result<Vec<u8>> {
    #[cfg(feature = "jpegli")]
    if encoder == Encoder::Cache {
        return jpegli_sys::encode_rgb(&img.px, img.w, img.h, JPEGLI_QUALITY)
            .map_err(|e| anyhow!("jpegli: {e}"));
    }
    let mut c = turbojpeg::Compressor::new()?;
    c.set_subsamp(Subsamp::Sub2x2)?;
    c.set_quality(if encoder == Encoder::Small { 70 } else { 80 })?;
    c.set_optimize(encoder != Encoder::Turbo)?;
    let image = turbojpeg::Image {
        pixels: &img.px[..],
        width: img.w as usize,
        pitch: img.w as usize * 3,
        height: img.h as usize,
        format: PixelFormat::RGB,
    };
    Ok(c.compress_to_vec(image)?)
}

/// The full pipeline after decoding.
pub fn finish(d: Decoded, size: u32, encoder: Encoder) -> anyhow::Result<Vec<u8>> {
    let mut img = resize(d.img, size)?;
    to_srgb(&mut img, d.icc.as_deref());
    let img = orient(img, d.orientation);
    encode(&img, encoder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(w: u32, h: u32) -> Rgb {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend([(x * 255 / w) as u8, (y * 255 / h) as u8, 90]);
            }
        }
        Rgb { w, h, px }
    }

    #[test]
    fn round_trip_with_scaling() {
        let jpg = encode(&checker(1600, 1200), Encoder::Turbo).unwrap();
        let d = decode_jpeg(&jpg, 320).unwrap();
        // 1/4 scale (400x300) is the smallest covering 320.
        assert_eq!((d.img.w, d.img.h), (400, 300));
        let out = finish(d, 320, Encoder::Cache).unwrap();
        let back = decode_jpeg(&out, 10_000).unwrap();
        assert_eq!((back.img.w, back.img.h), (320, 240));
        assert!(jpeg::exif(&out).is_none());
    }

    #[test]
    fn orientation_swaps_and_maps() {
        let mut img = Rgb {
            w: 2,
            h: 1,
            px: vec![1, 1, 1, 2, 2, 2],
        };
        img = orient(img, 6); // 90° clockwise: left pixel goes to the top
        assert_eq!((img.w, img.h), (1, 2));
        assert_eq!(img.px, vec![1, 1, 1, 2, 2, 2]);
        let img = orient(
            Rgb {
                w: 2,
                h: 1,
                px: vec![1, 1, 1, 2, 2, 2],
            },
            8,
        ); // 90° counter-clockwise
        assert_eq!(img.px, vec![2, 2, 2, 1, 1, 1]);
    }

    #[test]
    fn never_enlarges() {
        let img = resize(checker(100, 50), 640).unwrap();
        assert_eq!((img.w, img.h), (100, 50));
    }

    #[test]
    fn transparency_onto_grey() {
        assert_eq!(
            composite(&[255, 0, 0, 0, 255, 0, 0, 255]),
            vec![128, 128, 128, 255, 0, 0]
        );
    }
}
