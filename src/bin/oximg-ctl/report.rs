//! The fields every command reports about bytes it saw: a header probe
//! (format, stored size, animation, a JPEG's frame type) and a sha256.

use oximg::pipeline::{self, Animation};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn probe_value(bytes: &[u8]) -> Value {
    match pipeline::probe(bytes) {
        Ok((fmt, w, h)) => {
            let mut v = json!({
                "content_type": fmt.content_type(),
                "width": w,
                "height": h,
                "stored_pixels": w as u64 * h as u64,
            });
            match pipeline::probe_animation(bytes) {
                Ok(Some(Animation {
                    frames,
                    duration_ms,
                    loop_count,
                })) => {
                    v["animation"] = json!({
                        "frames": frames,
                        "duration_ms": duration_ms,
                        "loop_count": loop_count,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    v["error"] = json!(e.to_string());
                    v["animation_error"] = json!(e.to_string());
                }
            }
            if let Some(n) = jpeg_sof(bytes) {
                // Which frame the encoder chose is behavior the knobs
                // control (progressive vs sequential), so a proof needs
                // to see it without a second tool.
                v["jpeg"] = json!({"sof": n, "progressive": matches!(n, 2 | 6 | 10 | 14)});
            }
            v
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// The `n` of a JPEG's SOFn frame marker (0 baseline, 1 extended
/// sequential, 2 progressive, ...), found by walking the marker
/// segments that precede it. `None` for anything that is not a JPEG or
/// reaches the first scan without a frame header.
fn jpeg_sof(b: &[u8]) -> Option<u8> {
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    loop {
        if *b.get(i)? != 0xFF {
            return None;
        }
        // Any number of 0xFF fill bytes may precede a marker code.
        while *b.get(i + 1)? == 0xFF {
            i += 1;
        }
        let m = b[i + 1];
        match m {
            // DHT, JPG, DAC share the SOF range but are not frames.
            0xC4 | 0xC8 | 0xCC => {}
            0xC0..=0xCF => return Some(m - 0xC0),
            0xDA => return None,
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            _ => {}
        }
        let len = u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]) as usize;
        i += 2 + len;
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn probe_reports_type_and_stored_size() {
        let v = probe_value(&fixture("photo.jpg"));
        assert_eq!(v["content_type"], "image/jpeg");
        assert_eq!(
            (v["width"].as_u64(), v["height"].as_u64()),
            (Some(200), Some(150))
        );
        assert_eq!(v["stored_pixels"], 30_000);
        assert_eq!(v["jpeg"], json!({"sof": 0, "progressive": false}));
        assert!(v.get("animation").is_none(), "{v}");
        assert!(v.get("error").is_none(), "{v}");
    }

    #[test]
    fn probe_reports_the_jpeg_frame_type() {
        let v = probe_value(&fixture("cmyk_prog.jpg"));
        assert_eq!(v["jpeg"], json!({"sof": 2, "progressive": true}));
        // Non-JPEGs carry no `jpeg` object.
        assert!(probe_value(&fixture("rgb.png")).get("jpeg").is_none());
    }

    #[test]
    fn jpeg_sof_walks_fill_bytes_and_standalone_markers() {
        // SOI, fill bytes, RST0 (standalone), DHT (in the SOF range but
        // not a frame), then SOF1.
        let b = [
            0xFF, 0xD8, 0xFF, 0xFF, 0xD0, 0xFF, 0xC4, 0x00, 0x03, 0x00, 0xFF, 0xC1, 0x00, 0x02,
        ];
        assert_eq!(jpeg_sof(&b), Some(1));
        // A scan before any frame, garbage between segments, truncation,
        // and a non-JPEG all report nothing.
        assert_eq!(jpeg_sof(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]), None);
        assert_eq!(jpeg_sof(&[0xFF, 0xD8, 0x00]), None);
        assert_eq!(jpeg_sof(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]), None);
        assert_eq!(jpeg_sof(b"\x89PNG"), None);
    }

    #[test]
    fn probe_reports_animation() {
        let v = probe_value(&fixture("anim.gif"));
        assert_eq!(v["animation"]["frames"], 3);
        assert_eq!(v["animation"]["duration_ms"], 1500);
    }

    #[test]
    fn probe_of_non_image_bytes_is_an_error_field() {
        let v = probe_value(b"not an image");
        assert!(v["error"].is_string(), "{v}");
        assert!(v.get("width").is_none(), "{v}");
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
