//! Helpers for media carried as base64, without network or codec dependencies.
pub const DEFAULT_RESPONSES_IMAGE_DETAIL: &str = "auto";

/// Preserve supported image detail hints and default unknown hints to `auto`.
pub fn responses_image_detail(detail: &str) -> &'static str {
    ["low", "high", "auto", "original"]
        .into_iter()
        .find(|known| detail.trim().eq_ignore_ascii_case(known))
        .unwrap_or(DEFAULT_RESPONSES_IMAGE_DETAIL)
}

pub fn is_text_mime_type(mime: &str) -> bool {
    let s = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    s.starts_with("text/") || s == "application/json"
}
pub fn decode_text(data: &str) -> Option<String> {
    String::from_utf8(decode_base64(data.trim())?).ok()
}
fn decode_base64(data: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = data
        .bytes()
        .filter(|c| *c != b'\r' && *c != b'\n')
        .collect();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, b) in bytes.chunks_exact(4).enumerate() {
        let mut n = 0u32;
        let mut pad = 0;
        for (j, &c) in b.iter().enumerate() {
            let d = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' if j >= 2 => {
                    pad += 1;
                    0
                }
                _ => return None,
            };
            if pad > 0 && c != b'=' {
                return None;
            }
            n = (n << 6) | u32::from(d);
        }
        if pad > 0 && i + 1 != bytes.len() / 4 {
            return None;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8)
        }
        if pad < 1 {
            out.push(n as u8)
        }
    }
    Some(out)
}
