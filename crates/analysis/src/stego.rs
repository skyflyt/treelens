//! Steganography analysis toolkit — detect, extract, and (for round-trip
//! testing / watermarking your own files) embed hidden data using three
//! classic techniques:
//!
//! 1. **LSB** — least-significant-bit embedding in PNG/BMP pixel data.
//! 2. **Whitespace / SNOW** — trailing space/tab sequences after text lines
//!    (the technique the classic `snow` tool uses).
//! 3. **Format-based** — payload appended after a file's logical end-of-image
//!    marker (PNG `IEND`, JPEG `EOI`), invisible to viewers.
//!
//! All three share a common framed-payload layout so extraction is
//! unambiguous and detection has a concrete signal to look for:
//!
//! ```text
//! magic  "TLNS"  (4 bytes)
//! len    u32 LE  (payload length in bytes)
//! payload[len]
//! ```
//!
//! This is a local forensic/analysis tool: it reads and writes files the user
//! selects on their own machine. Nothing leaves the machine.

use crate::{AnalysisError, Result, Verdict};
use serde::Serialize;
use std::path::Path;

pub const MAGIC: &[u8; 4] = b"TLNS";
const HEADER_LEN: usize = 8; // 4 magic + 4 length

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Lsb,
    Whitespace,
    FormatAppend,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanFinding {
    pub method: Method,
    /// True only when hidden data is actually *recoverable* — a definitive
    /// signal with no false positives (a framed payload was found, or trailing
    /// data exists after the format's logical end). This is the headline
    /// "detect & reverse" result.
    pub suspicious: bool,
    pub confidence: f32,
    /// A weaker, advisory statistical signal (e.g. LSB histogram-pair
    /// equalization) that suggests *some* tool may have embedded data even when
    /// we can't recover it. Heuristic — may have false positives/negatives on
    /// small or synthetic images. Never folded into `suspicious`.
    pub statistical_anomaly: bool,
    /// Human-readable note (what was seen, e.g. "framed payload: 124 bytes").
    pub detail: String,
    /// If a framed Treelens payload was recoverable, its byte length.
    pub recoverable_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub path: String,
    pub findings: Vec<ScanFinding>,
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
    v.extend_from_slice(MAGIC);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

/// Parse a framed payload from the front of `bytes`. Returns the payload if the
/// magic matches and the declared length fits.
fn unframe(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < HEADER_LEN || &bytes[0..4] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let end = HEADER_LEN.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    Some(bytes[HEADER_LEN..end].to_vec())
}

// ============================ LSB (images) ============================

mod lsb {
    use super::*;
    use image::{GenericImageView, ImageReader};

    /// Read an image's RGB bytes (ignoring alpha) as the LSB carrier stream.
    fn load_rgb(path: &Path) -> Result<(Vec<u8>, u32, u32, image::DynamicImage)> {
        let img = ImageReader::open(path)?
            .with_guessed_format()?
            .decode()
            .map_err(|e| AnalysisError::Image(e.to_string()))?;
        let (w, h) = img.dimensions();
        let rgb = img.to_rgb8();
        Ok((rgb.into_raw(), w, h, img))
    }

    /// Capacity in payload bytes for an image: one bit per RGB sample, minus
    /// the framing header.
    pub fn capacity_bytes(rgb_len: usize) -> u64 {
        let total_bits = rgb_len; // 1 bit per byte/sample
        let total_bytes = total_bits / 8;
        total_bytes.saturating_sub(HEADER_LEN) as u64
    }

    /// Embed `payload` into the LSBs of an image's RGB samples, writing a PNG to
    /// `out` (PNG so the LSBs survive — never re-encode to a lossy format).
    pub fn embed(src: &Path, out: &Path, payload: &[u8]) -> Result<()> {
        let (mut rgb, w, h, _img) = load_rgb(src)?;
        let framed = frame(payload);
        let need_bits = framed.len() * 8;
        if need_bits > rgb.len() {
            return Err(AnalysisError::Capacity {
                payload: payload.len() as u64,
                capacity: capacity_bytes(rgb.len()),
            });
        }
        let mut bit = 0usize;
        for byte in &framed {
            for k in 0..8 {
                let b = (byte >> k) & 1;
                rgb[bit] = (rgb[bit] & 0xFE) | b;
                bit += 1;
            }
        }
        image::save_buffer(out, &rgb, w, h, image::ColorType::Rgb8)
            .map_err(|e| AnalysisError::Image(e.to_string()))?;
        Ok(())
    }

    /// Read the LSB stream and try to recover a framed payload.
    pub fn extract(path: &Path) -> Result<Vec<u8>> {
        let (rgb, _w, _h, _img) = load_rgb(path)?;
        // First recover the 8-byte header to learn the length, then the body.
        let header = bits_to_bytes(&rgb, 0, HEADER_LEN);
        if &header[0..4] != MAGIC {
            return Err(AnalysisError::NotFound);
        }
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let total = HEADER_LEN + len;
        if total * 8 > rgb.len() {
            return Err(AnalysisError::NotFound);
        }
        let all = bits_to_bytes(&rgb, 0, total);
        unframe(&all).ok_or(AnalysisError::NotFound)
    }

    fn bits_to_bytes(rgb: &[u8], start_byte: usize, n_bytes: usize) -> Vec<u8> {
        let mut out = vec![0u8; n_bytes];
        let mut bit = start_byte * 8;
        for byte_out in out.iter_mut() {
            for k in 0..8 {
                if bit >= rgb.len() {
                    return out;
                }
                let b = rgb[bit] & 1;
                *byte_out |= b << k;
                bit += 1;
            }
        }
        out
    }

    /// Detection: (1) probe for our framed magic; (2) measure the entropy /
    /// balance of the LSB plane. A clean photograph's LSB plane is noisy but a
    /// freshly LSB-embedded region is *maximally* balanced (≈50/50) and high
    /// entropy — combined with a magic hit that's a strong signal.
    pub fn detect(path: &Path) -> ScanFinding {
        let loaded = load_rgb(path);
        let (rgb, _w, _h, _img) = match loaded {
            Ok(v) => v,
            Err(_) => {
                return ScanFinding {
                    method: Method::Lsb,
                    suspicious: false,
                    confidence: 0.0,
                    statistical_anomaly: false,
                    detail: "not a decodable PNG/BMP image".into(),
                    recoverable_bytes: None,
                };
            }
        };

        // Magic probe — the reliable, recoverable signal.
        let header = bits_to_bytes(&rgb, 0, HEADER_LEN);
        if &header[0..4] == MAGIC {
            let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            return ScanFinding {
                method: Method::Lsb,
                suspicious: true,
                confidence: 0.99,
                statistical_anomaly: true,
                detail: format!("Treelens LSB payload header found ({len} bytes) — extractable"),
                recoverable_bytes: Some(len as u64),
            };
        }

        // Statistical advisory: chi-square "pairs of values" test. Never sets
        // `suspicious` (it's not recoverable / can false-positive); it only
        // raises the advisory flag.
        let v = chi_square_verdict(&rgb);
        ScanFinding {
            method: Method::Lsb,
            suspicious: false,
            confidence: v.confidence,
            statistical_anomaly: v.suspicious,
            detail: if v.suspicious {
                "no Treelens payload, but LSB histogram pairs are equalized — possible LSB stego by another tool (advisory)".into()
            } else {
                "no hidden payload; LSB histogram looks natural".into()
            },
            recoverable_bytes: None,
        }
    }

    /// Chi-square "pairs of values" attack (Westfeld–Pfitzmann). Sequential
    /// LSB-replacement embedding equalizes the histogram counts of each value
    /// pair (2i, 2i+1). We measure the mean relative difference within pairs:
    /// natural images keep pairs distinct (high mean diff); embedded regions
    /// flatten them toward equal (low mean diff). This does NOT fire on smooth
    /// natural photos and is a far better signal than raw bit-balance. It's an
    /// *advisory* heuristic — the reliable detector is the magic-header probe.
    fn chi_square_verdict(rgb: &[u8]) -> Verdict {
        if rgb.len() < 8192 {
            return Verdict {
                suspicious: false,
                confidence: 0.0,
            };
        }
        let mut hist = [0u64; 256];
        for &b in rgb {
            hist[b as usize] += 1;
        }
        let mut sum_rel = 0.0f64;
        let mut sum_total = 0.0f64;
        let mut pairs = 0u32;
        for i in 0..128 {
            let a = hist[2 * i] as f64;
            let b = hist[2 * i + 1] as f64;
            let total = a + b;
            // Only consider pairs with enough mass to be statistically meaningful.
            if total >= 32.0 {
                sum_rel += (a - b).abs() / total;
                sum_total += total;
                pairs += 1;
            }
        }
        if pairs < 16 {
            return Verdict {
                suspicious: false,
                confidence: 0.0,
            };
        }
        let mean_rel = sum_rel / pairs as f64; // ~0 when pairs equalized
                                               // Expected relative difference of a *fair* (equalized) split of N items
                                               // is √(2/πN). LSB-replaced histograms collapse to roughly that noise
                                               // floor; natural images sit several times above it. Scaling the
                                               // threshold by the actual per-pair sample size makes this work at any
                                               // image resolution instead of a fixed cutoff that only fits one size.
        let avg_total = sum_total / pairs as f64;
        let noise_floor = (2.0 / (std::f64::consts::PI * avg_total)).sqrt();
        let threshold = noise_floor * 2.0;
        if mean_rel < threshold {
            // Closer to the noise floor → higher confidence (capped; this is an
            // advisory heuristic, never as certain as the magic-header probe).
            let ratio = (mean_rel / threshold).clamp(0.0, 1.0);
            let conf = (0.85 - ratio * 0.35) as f32;
            Verdict {
                suspicious: true,
                confidence: conf,
            }
        } else {
            Verdict {
                suspicious: false,
                confidence: 0.0,
            }
        }
    }
}

// ====================== Whitespace / SNOW (text) ======================

mod whitespace {
    use super::*;

    // Encode each payload bit as a trailing whitespace char appended to text
    // lines: space = 0, tab = 1. Decoded by reading the trailing run.
    const BIT0: u8 = b' ';
    const BIT1: u8 = b'\t';

    /// Capacity: one bit per line (we append one whitespace char per line).
    pub fn capacity_bytes(text: &str) -> u64 {
        let lines = text.lines().count().max(1);
        ((lines / 8) as u64).saturating_sub(HEADER_LEN as u64)
    }

    pub fn embed(src: &Path, out: &Path, payload: &[u8]) -> Result<()> {
        let text = std::fs::read_to_string(src)
            .map_err(|_| AnalysisError::Unsupported("not a UTF-8 text file".into()))?;
        let framed = frame(payload);
        let bits: Vec<u8> = framed
            .iter()
            .flat_map(|b| (0..8).map(move |k| (b >> k) & 1))
            .collect();

        let lines: Vec<&str> = text.split('\n').collect();
        // We need at least `bits.len()` lines to carry the payload.
        if bits.len() > lines.len() {
            return Err(AnalysisError::Capacity {
                payload: payload.len() as u64,
                capacity: capacity_bytes(&text),
            });
        }
        let mut out_s = String::with_capacity(text.len() + bits.len());
        for (i, line) in lines.iter().enumerate() {
            // Strip any existing trailing space/tab so re-embedding is clean.
            let trimmed = line.trim_end_matches([' ', '\t']);
            out_s.push_str(trimmed);
            if i < bits.len() {
                out_s.push(if bits[i] == 1 {
                    BIT1 as char
                } else {
                    BIT0 as char
                });
            }
            if i < lines.len() - 1 {
                out_s.push('\n');
            }
        }
        std::fs::write(out, out_s)?;
        Ok(())
    }

    pub fn extract(path: &Path) -> Result<Vec<u8>> {
        let text = std::fs::read_to_string(path)
            .map_err(|_| AnalysisError::Unsupported("not a UTF-8 text file".into()))?;
        let mut bits: Vec<u8> = Vec::new();
        for line in text.split('\n') {
            // Look at the final trailing whitespace char, if any.
            match line.chars().last() {
                Some(' ') => bits.push(0),
                Some('\t') => bits.push(1),
                _ => {} // no carrier on this line
            }
        }
        let bytes = bits_to_bytes(&bits);
        unframe(&bytes).ok_or(AnalysisError::NotFound)
    }

    fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; bits.len() / 8];
        for (i, chunk) in bits.chunks(8).enumerate() {
            if chunk.len() < 8 {
                break;
            }
            let mut byte = 0u8;
            for (k, b) in chunk.iter().enumerate() {
                byte |= (b & 1) << k;
            }
            out[i] = byte;
        }
        out
    }

    pub fn detect(path: &Path) -> ScanFinding {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => {
                return ScanFinding {
                    method: Method::Whitespace,
                    suspicious: false,
                    confidence: 0.0,
                    statistical_anomaly: false,
                    detail: "not a UTF-8 text file".into(),
                    recoverable_bytes: None,
                };
            }
        };
        // Magic probe via extraction — the reliable, recoverable signal.
        if let Ok(p) = extract(path) {
            return ScanFinding {
                method: Method::Whitespace,
                suspicious: true,
                confidence: 0.99,
                statistical_anomaly: true,
                detail: format!(
                    "Treelens whitespace payload recovered ({} bytes) — extractable",
                    p.len()
                ),
                recoverable_bytes: Some(p.len() as u64),
            };
        }
        // Advisory: fraction of lines ending in trailing whitespace. Source code
        // occasionally has trailing whitespace, but a high fraction is the SNOW
        // signature. Advisory only — we couldn't recover a framed payload.
        let mut carrier = 0usize;
        let mut total = 0usize;
        for line in text.split('\n') {
            if line.is_empty() {
                continue;
            }
            total += 1;
            if matches!(line.chars().last(), Some(' ') | Some('\t')) {
                carrier += 1;
            }
        }
        let frac = if total > 0 {
            carrier as f64 / total as f64
        } else {
            0.0
        };
        let anomaly = total >= 16 && frac > 0.5;
        ScanFinding {
            method: Method::Whitespace,
            suspicious: false,
            confidence: if anomaly {
                (frac as f32).min(0.85)
            } else {
                0.0
            },
            statistical_anomaly: anomaly,
            detail: if anomaly {
                format!("no Treelens payload, but {carrier}/{total} lines carry trailing whitespace — possible whitespace stego (advisory)")
            } else {
                format!(
                    "no hidden payload; {carrier}/{total} lines have trailing whitespace (normal)"
                )
            },
            recoverable_bytes: None,
        }
    }
}

// ===================== Format-based (append after EOF) =====================

mod format_append {
    use super::*;

    /// Locate the logical end-of-file for known image formats. Returns the
    /// offset where trailing/appended data would begin, or None if the format
    /// isn't recognized (in which case appended data can't be distinguished
    /// from the file's own content).
    fn logical_end(bytes: &[u8]) -> Option<(usize, &'static str)> {
        // PNG: ends with the IEND chunk: ... 00 00 00 00 'IEND' <crc:4>.
        if bytes.len() > 8 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
            if let Some(pos) = find_subsequence(bytes, b"IEND") {
                // IEND chunk = 'IEND' + 4-byte CRC; logical end is pos+4+4.
                let end = pos + 4 + 4;
                if end <= bytes.len() {
                    return Some((end, "PNG (after IEND)"));
                }
            }
        }
        // JPEG: starts FF D8, ends with EOI marker FF D9.
        if bytes.len() > 2 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
            if let Some(pos) = rfind_subsequence(bytes, &[0xFF, 0xD9]) {
                return Some((pos + 2, "JPEG (after EOI)"));
            }
        }
        // GIF: ends with trailer 0x3B.
        if bytes.len() > 6 && (&bytes[0..6] == b"GIF89a" || &bytes[0..6] == b"GIF87a") {
            if let Some(pos) = bytes.iter().rposition(|&b| b == 0x3B) {
                return Some((pos + 1, "GIF (after trailer)"));
            }
        }
        None
    }

    pub fn embed(src: &Path, out: &Path, payload: &[u8]) -> Result<()> {
        let mut bytes = std::fs::read(src)?;
        let (end, _fmt) = logical_end(&bytes).ok_or_else(|| {
            AnalysisError::Unsupported("unrecognized container format (need PNG/JPEG/GIF)".into())
        })?;
        // Truncate any existing trailing data, then append our framed payload.
        bytes.truncate(end);
        bytes.extend_from_slice(&frame(payload));
        std::fs::write(out, bytes)?;
        Ok(())
    }

    pub fn extract(path: &Path) -> Result<Vec<u8>> {
        let bytes = std::fs::read(path)?;
        let (end, _fmt) = logical_end(&bytes).ok_or(AnalysisError::NotFound)?;
        if end >= bytes.len() {
            return Err(AnalysisError::NotFound);
        }
        unframe(&bytes[end..]).ok_or(AnalysisError::NotFound)
    }

    pub fn detect(path: &Path) -> ScanFinding {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                return ScanFinding {
                    method: Method::FormatAppend,
                    suspicious: false,
                    confidence: 0.0,
                    statistical_anomaly: false,
                    detail: "could not read file".into(),
                    recoverable_bytes: None,
                };
            }
        };
        match logical_end(&bytes) {
            Some((end, fmt)) if end < bytes.len() => {
                let trailing = bytes.len() - end;
                let framed = unframe(&bytes[end..]);
                ScanFinding {
                    method: Method::FormatAppend,
                    // Trailing data after the logical EOF is real and extractable
                    // regardless of framing, so this is a definitive signal.
                    suspicious: true,
                    confidence: if framed.is_some() { 0.99 } else { 0.8 },
                    statistical_anomaly: true,
                    detail: match &framed {
                        Some(p) => format!("{fmt}: {} bytes appended after EOF — Treelens payload, {} bytes (extractable)", trailing, p.len()),
                        None => format!("{fmt}: {trailing} bytes of data after the logical end of the file (extractable as raw bytes)"),
                    },
                    recoverable_bytes: framed.map(|p| p.len() as u64),
                }
            }
            Some((_, fmt)) => ScanFinding {
                method: Method::FormatAppend,
                suspicious: false,
                confidence: 0.0,
                statistical_anomaly: false,
                detail: format!("{fmt}: no trailing data"),
                recoverable_bytes: None,
            },
            None => ScanFinding {
                method: Method::FormatAppend,
                suspicious: false,
                confidence: 0.0,
                statistical_anomaly: false,
                detail: "format not recognized (PNG/JPEG/GIF only)".into(),
                recoverable_bytes: None,
            },
        }
    }

    fn find_subsequence(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }
    fn rfind_subsequence(hay: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.len() > hay.len() {
            return None;
        }
        (0..=hay.len() - needle.len())
            .rev()
            .find(|&i| &hay[i..i + needle.len()] == needle)
    }
}

// ============================== Public API ==============================

/// Run every detector against a file and return a combined report.
pub fn scan(path: impl AsRef<Path>) -> ScanReport {
    let p = path.as_ref();
    ScanReport {
        path: p.to_string_lossy().to_string(),
        findings: vec![
            lsb::detect(p),
            whitespace::detect(p),
            format_append::detect(p),
        ],
    }
}

/// Embed `payload` into `src` using `method`, writing the result to `out`.
pub fn embed(
    method: Method,
    src: impl AsRef<Path>,
    out: impl AsRef<Path>,
    payload: &[u8],
) -> Result<()> {
    let (src, out) = (src.as_ref(), out.as_ref());
    match method {
        Method::Lsb => lsb::embed(src, out, payload),
        Method::Whitespace => whitespace::embed(src, out, payload),
        Method::FormatAppend => format_append::embed(src, out, payload),
    }
}

/// Extract a framed Treelens payload from `src` using `method`.
pub fn extract(method: Method, src: impl AsRef<Path>) -> Result<Vec<u8>> {
    let src = src.as_ref();
    match method {
        Method::Lsb => lsb::extract(src),
        Method::Whitespace => whitespace::extract(src),
        Method::FormatAppend => format_append::extract(src),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn make_png(path: &Path, w: u32, h: u32) {
        // A photo-like image: a smooth gradient base plus deterministic per-pixel
        // noise (a simple LCG, no Math.random). This gives a *natural* histogram
        // whose value-pairs are NOT equalized, so the chi-square detector treats
        // it as clean — unlike a uniform gradient, which would be pathological.
        let mut img = image::RgbImage::new(w, h);
        let mut s: u32 = 0x1234_5678;
        let mut rng = || {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        };
        for (x, y, px) in img.enumerate_pixels_mut() {
            let base_r = (x.wrapping_mul(3) % 200) as u8;
            let base_g = (y.wrapping_mul(2) % 200) as u8;
            let base_b = ((x + y) % 200) as u8;
            // Add noise but keep it bounded so pairs stay naturally skewed.
            *px = image::Rgb([
                base_r.wrapping_add(rng() % 40),
                base_g.wrapping_add(rng() % 40),
                base_b.wrapping_add(rng() % 40),
            ]);
        }
        img.save(path).unwrap();
    }

    #[test]
    fn lsb_round_trip() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("cover.png");
        let out = dir.path().join("stego.png");
        make_png(&src, 64, 64);

        let secret = b"the eagle lands at midnight";
        embed(Method::Lsb, &src, &out, secret).unwrap();
        let got = extract(Method::Lsb, &out).unwrap();
        assert_eq!(got, secret);

        // Detector flags the stego image, not the clean cover.
        let clean = scan(&src);
        let lsb_clean = clean
            .findings
            .iter()
            .find(|f| f.method == Method::Lsb)
            .unwrap();
        assert!(!lsb_clean.suspicious, "clean cover should not be flagged");

        let dirty = scan(&out);
        let lsb_dirty = dirty
            .findings
            .iter()
            .find(|f| f.method == Method::Lsb)
            .unwrap();
        assert!(lsb_dirty.suspicious, "stego image should be flagged");
        assert_eq!(lsb_dirty.recoverable_bytes, Some(secret.len() as u64));
    }

    #[test]
    fn whitespace_round_trip() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("doc.txt");
        let out = dir.path().join("doc_stego.txt");
        // 400 lines of clean prose — plenty of carrier capacity.
        let body: String = (0..400)
            .map(|i| format!("This is line number {i}.\n"))
            .collect();
        std::fs::write(&src, body).unwrap();

        let secret = b"hidden in plain sight";
        embed(Method::Whitespace, &src, &out, secret).unwrap();
        let got = extract(Method::Whitespace, &out).unwrap();
        assert_eq!(got, secret);

        let report = scan(&out);
        let ws = report
            .findings
            .iter()
            .find(|f| f.method == Method::Whitespace)
            .unwrap();
        assert!(ws.suspicious);
        assert_eq!(ws.recoverable_bytes, Some(secret.len() as u64));
    }

    #[test]
    fn format_append_round_trip() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("pic.png");
        let out = dir.path().join("pic_stego.png");
        make_png(&src, 16, 16);

        let secret = b"appended after IEND";
        embed(Method::FormatAppend, &src, &out, secret).unwrap();
        let got = extract(Method::FormatAppend, &out).unwrap();
        assert_eq!(got, secret);

        let report = scan(&out);
        let fa = report
            .findings
            .iter()
            .find(|f| f.method == Method::FormatAppend)
            .unwrap();
        assert!(fa.suspicious);
        assert_eq!(fa.recoverable_bytes, Some(secret.len() as u64));

        // Clean PNG has no trailing data.
        let clean = scan(&src);
        let fa_clean = clean
            .findings
            .iter()
            .find(|f| f.method == Method::FormatAppend)
            .unwrap();
        assert!(!fa_clean.suspicious);
    }

    #[test]
    fn chi_square_flags_heavy_raw_lsb_embedding() {
        // Simulate a different tool's LSB stego: overwrite EVERY sample's LSB
        // with pseudo-random bits (no Treelens magic). The chi-square detector
        // should flag it via histogram-pair equalization, even though the magic
        // probe finds nothing.
        let dir = tempdir().unwrap();
        let src = dir.path().join("cover.png");
        let out = dir.path().join("raw_lsb.png");
        make_png(&src, 96, 96);

        let img = image::ImageReader::open(&src).unwrap().decode().unwrap();
        let (w, h) = (img.width(), img.height());
        let mut rgb = img.to_rgb8().into_raw();
        let mut s: u32 = 0xC0FF_EE11;
        for byte in rgb.iter_mut() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (*byte & 0xFE) | ((s >> 31) as u8);
        }
        image::save_buffer(&out, &rgb, w, h, image::ColorType::Rgb8).unwrap();

        let report = scan(&out);
        let lsb = report
            .findings
            .iter()
            .find(|f| f.method == Method::Lsb)
            .unwrap();
        assert!(
            lsb.statistical_anomaly,
            "heavy raw LSB embedding should trip the chi-square advisory: {}",
            lsb.detail
        );
        assert!(
            !lsb.suspicious,
            "raw embedding has no recoverable framed payload"
        );
        assert_eq!(
            lsb.recoverable_bytes, None,
            "no Treelens magic in raw embedding"
        );
    }

    #[test]
    fn extract_finds_nothing_in_clean_file() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("plain.txt");
        let mut fh = std::fs::File::create(&f).unwrap();
        fh.write_all(b"nothing to see here\njust text\n").unwrap();
        assert!(matches!(
            extract(Method::Whitespace, &f),
            Err(AnalysisError::NotFound)
        ));
    }
}
