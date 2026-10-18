//! jpegli's encoder for 8-bit RGB, statically linked (vendor/VERSIONS has the commits).

use std::ffi::{CStr, c_char, c_int};

const JMSG_LENGTH_MAX: usize = 200;

unsafe extern "C" {
    fn wirephoto_jpegli_encode_rgb(
        px: *const u8,
        width: u32,
        height: u32,
        quality: c_int,
        out: *mut *mut u8,
        out_len: *mut usize,
        msg: *mut c_char,
    ) -> c_int;
    fn wirephoto_jpegli_free(p: *mut u8);
}

/// Encodes tightly packed RGB as a baseline 4:2:0 JPEG with no metadata, at a libjpeg-style `quality` (mapped to
/// a butteraugli distance the way cjpegli's `-q` is).
pub fn encode_rgb(px: &[u8], width: u32, height: u32, quality: i32) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 || px.len() != width as usize * height as usize * 3 {
        return Err(format!("{} bytes is not {width}x{height} RGB", px.len()));
    }
    let mut out = std::ptr::null_mut();
    let mut len = 0;
    let mut msg = [0 as c_char; JMSG_LENGTH_MAX];
    // SAFETY: px holds width * height * 3 bytes; the shim catches jpegli's errors (setjmp) and returns a malloc'd
    // buffer of `len` bytes that is copied here and freed with the matching free.
    unsafe {
        if wirephoto_jpegli_encode_rgb(
            px.as_ptr(),
            width,
            height,
            quality,
            &mut out,
            &mut len,
            msg.as_mut_ptr(),
        ) != 0
        {
            return Err(CStr::from_ptr(msg.as_ptr()).to_string_lossy().into_owned());
        }
        let v = std::slice::from_raw_parts(out, len).to_vec();
        wirephoto_jpegli_free(out);
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_baseline_420() {
        let (w, h) = (97, 61);
        let px: Vec<u8> = (0..w * h * 3).map(|i| (i * 7 % 251) as u8).collect();
        let jpg = super::encode_rgb(&px, w, h, 80).unwrap();
        assert_eq!(&jpg[..2], &[0xFF, 0xD8]);
        assert!(jpg.windows(2).any(|m| m == [0xFF, 0xC0]), "baseline SOF0");
        assert!(!jpg.windows(2).any(|m| m == [0xFF, 0xE0]), "no JFIF");
        assert!(super::encode_rgb(&px, w, h + 1, 80).is_err());
    }
}
