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

        // Statistical advisory: windowed chi-square "pairs of values" test.
        // Never sets `suspicious` (it's not recoverable / can false-positive);
        // it only raises the advisory flag.
        let v = chi_square_windowed(&rgb);
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

    /// Run the chi-square pairs test over sliding windows of the carrier and
    /// take the strongest anomalous window. A payload embedded into only part of
    /// an image (the common case — sequential embedding fills from the top and
    /// stops) barely moves the *global* histogram, so a whole-image test misses
    /// it; a window that lands inside the embedded region is fully equalized and
    /// trips. Falls back to a single pass for carriers smaller than one window.
    fn chi_square_windowed(rgb: &[u8]) -> Verdict {
        // 64 KiB windows: large enough that each value-pair carries enough mass
        // (~256 samples/pair) for the noise floor to sit well below a natural
        // image's pair imbalance, so a clean image doesn't false-positive while
        // a window sitting inside a random-LSB region (imbalance ≈ noise floor)
        // still trips.
        const WIN: usize = 64 * 1024;
        if rgb.len() < WIN {
            return chi_square_verdict(rgb);
        }
        let mut best = Verdict {
            suspicious: false,
            confidence: 0.0,
        };
        for chunk in rgb.chunks(WIN) {
            if chunk.len() < 8192 {
                continue; // tail too small to be meaningful
            }
            let v = chi_square_verdict(chunk);
            if v.suspicious && v.confidence > best.confidence {
                best = v;
            }
        }
        best
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
    /// Counts lines the same way `embed` does — `split('\n')`, which yields one
    /// carrier slot per newline-delimited segment (including a trailing empty
    /// segment after a final newline). Using `lines()` here under-counted by
    /// dropping that trailing segment, so the reported capacity disagreed with
    /// what `embed` would actually accept.
    pub fn capacity_bytes(text: &str) -> u64 {
        let lines = text.split('\n').count();
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
    ///
    /// These walk the actual container structure rather than substring-searching
    /// for the end marker. A naive search is wrong both ways: the marker bytes
    /// (`IEND`, `FFD9`, `0x3B`) routinely occur inside compressed pixel/entropy
    /// data, and they can also occur inside an appended payload — so neither a
    /// forward nor a reverse substring search reliably finds the *real* logical
    /// end. Parsing the structure pins it exactly.
    fn logical_end(bytes: &[u8]) -> Option<(usize, &'static str)> {
        if let Some(end) = png_logical_end(bytes) {
            return Some((end, "PNG (after IEND)"));
        }
        if let Some(end) = jpeg_logical_end(bytes) {
            return Some((end, "JPEG (after EOI)"));
        }
        if let Some(end) = gif_logical_end(bytes) {
            return Some((end, "GIF (after trailer)"));
        }
        None
    }

    /// Walk PNG chunks (length:4 BE, type:4, data:len, crc:4) from the 8-byte
    /// signature to the IEND chunk; the logical end is just past IEND's CRC.
    fn png_logical_end(bytes: &[u8]) -> Option<usize> {
        if bytes.len() < 8 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
            return None;
        }
        let mut pos = 8usize;
        loop {
            // Need length(4) + type(4) before we can size the chunk.
            if pos + 8 > bytes.len() {
                return None; // truncated header
            }
            let len = u32::from_be_bytes([
                bytes[pos],
                bytes[pos + 1],
                bytes[pos + 2],
                bytes[pos + 3],
            ]) as usize;
            let ctype = &bytes[pos + 4..pos + 8];
            // total chunk = len(4) + type(4) + data(len) + crc(4)
            let chunk_end = pos.checked_add(12)?.checked_add(len)?;
            if chunk_end > bytes.len() {
                return None; // truncated/invalid
            }
            if ctype == b"IEND" {
                return Some(chunk_end);
            }
            pos = chunk_end;
        }
    }

    /// Walk JPEG markers from SOI (FFD8) to EOI (FFD9), stepping over segment
    /// lengths and scanning entropy-coded data after each SOS for the next
    /// marker (handling restart markers and progressive multi-scan files).
    fn jpeg_logical_end(bytes: &[u8]) -> Option<usize> {
        if bytes.len() < 2 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
            return None;
        }
        let mut pos = 2usize;
        loop {
            if pos + 1 >= bytes.len() {
                return None;
            }
            if bytes[pos] != 0xFF {
                return None; // desynced — not a clean JPEG
            }
            // Skip fill bytes (a run of 0xFF is allowed before a marker).
            let mut mpos = pos + 1;
            while mpos < bytes.len() && bytes[mpos] == 0xFF {
                mpos += 1;
            }
            if mpos >= bytes.len() {
                return None;
            }
            let marker = bytes[mpos];
            pos = mpos + 1;
            match marker {
                0xD9 => return Some(pos),                 // EOI
                0xD8 | 0x01 => continue,                  // SOI / TEM: no payload
                0xD0..=0xD7 => continue,                  // RSTn: no payload
                0xDA => {
                    // SOS: 2-byte header length, then entropy data until next marker.
                    if pos + 2 > bytes.len() {
                        return None;
                    }
                    let seg_len =
                        u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
                    pos = pos.checked_add(seg_len)?;
                    if pos > bytes.len() {
                        return None;
                    }
                    while pos + 1 < bytes.len() {
                        if bytes[pos] == 0xFF
                            && bytes[pos + 1] != 0x00
                            && !(0xD0..=0xD7).contains(&bytes[pos + 1])
                        {
                            break; // found the next marker
                        }
                        pos += 1;
                    }
                }
                _ => {
                    // Marker with a 2-byte length field.
                    if pos + 2 > bytes.len() {
                        return None;
                    }
                    let seg_len =
                        u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
                    if seg_len < 2 {
                        return None;
                    }
                    pos = pos.checked_add(seg_len)?;
                    if pos > bytes.len() {
                        return None;
                    }
                }
            }
        }
    }

    /// Walk GIF blocks from the header past the (optional) global color table,
    /// over each extension / image-descriptor block's sub-blocks, to the 0x3B
    /// trailer; logical end is just past it.
    fn gif_logical_end(bytes: &[u8]) -> Option<usize> {
        if bytes.len() < 13 || (&bytes[0..6] != b"GIF89a" && &bytes[0..6] != b"GIF87a") {
            return None;
        }
        // Logical Screen Descriptor packed field is the 5th byte after the header.
        let packed = bytes[10];
        let mut pos = 13usize;
        if packed & 0x80 != 0 {
            let gct_entries = 1usize << (((packed & 0x07) + 1) as usize);
            pos = pos.checked_add(3 * gct_entries)?;
        }
        loop {
            if pos >= bytes.len() {
                return None;
            }
            match bytes[pos] {
                0x3B => return Some(pos + 1), // trailer
                0x21 => {
                    // Extension: introducer + label, then sub-blocks.
                    pos = pos.checked_add(2)?;
                    pos = gif_skip_sub_blocks(bytes, pos)?;
                }
                0x2C => {
                    // Image descriptor: 10 bytes, optional local color table,
                    // LZW min-code-size byte, then image data sub-blocks.
                    if pos + 10 > bytes.len() {
                        return None;
                    }
                    let lpacked = bytes[pos + 9];
                    pos += 10;
                    if lpacked & 0x80 != 0 {
                        let lct_entries = 1usize << (((lpacked & 0x07) + 1) as usize);
                        pos = pos.checked_add(3 * lct_entries)?;
                    }
                    if pos >= bytes.len() {
                        return None;
                    }
                    pos += 1; // LZW minimum code size
                    pos = gif_skip_sub_blocks(bytes, pos)?;
                }
                _ => return None, // unknown block
            }
        }
    }

    /// Advance past a GIF sub-block list (each: length byte + data, terminated
    /// by a zero-length block). Returns the offset just past the terminator.
    fn gif_skip_sub_blocks(bytes: &[u8], mut pos: usize) -> Option<usize> {
        loop {
            if pos >= bytes.len() {
                return None;
            }
            let n = bytes[pos] as usize;
            pos += 1;
            if n == 0 {
                return Some(pos);
            }
            pos = pos.checked_add(n)?;
            if pos > bytes.len() {
                return None;
            }
        }
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
        // A clean baseline whose LSB plane is *structured*, not random: each
        // channel is a peaked (triangular) draw forced to an even value, so the
        // LSB is always 0. The chi-square pairs test keys on whether the counts
        // of value 2i and 2i+1 have been equalized by LSB randomization; here
        // every odd bin is empty, so pairs are maximally imbalanced and the
        // image reads as clean. LSB-replacement embedding (random LSBs) is what
        // equalizes the pairs and trips the detector — which is exactly the
        // signal we want to isolate. (A smooth-but-noisy histogram would have
        // near-equal adjacent bins and is a known false-positive mode for this
        // family of detectors, so we don't model the clean image that way.)
        let mut img = image::RgbImage::new(w, h);
        let mut s: u32 = 0x1234_5678;
        let mut rng = || {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        };
        let mut draw = || ((64u16 + (rng() as u16 % 32) + (rng() as u16 % 32)) & 0xFE) as u8;
        for (_x, _y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([draw(), draw(), draw()]);
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

    #[test]
    fn clean_large_image_is_not_a_chi_square_false_positive() {
        // A natural-looking image well past the windowing threshold must not
        // raise the advisory flag in any window.
        let dir = tempdir().unwrap();
        let src = dir.path().join("big.png");
        make_png(&src, 256, 256);
        let report = scan(&src);
        let lsb = report
            .findings
            .iter()
            .find(|f| f.method == Method::Lsb)
            .unwrap();
        assert!(
            !lsb.statistical_anomaly,
            "clean image tripped chi-square: {}",
            lsb.detail
        );
        assert!(!lsb.suspicious);
    }

    #[test]
    fn partial_lsb_embed_trips_windowed_chi_square() {
        // Overwrite the LSBs of only the first half of a large image. The global
        // histogram barely moves, but a window inside the embedded region is
        // fully equalized — the windowed test must catch it.
        let dir = tempdir().unwrap();
        let src = dir.path().join("cover.png");
        let out = dir.path().join("partial.png");
        make_png(&src, 256, 256);

        let img = image::ImageReader::open(&src).unwrap().decode().unwrap();
        let (w, h) = (img.width(), img.height());
        let mut rgb = img.to_rgb8().into_raw();
        let half = rgb.len() / 2;
        let mut s: u32 = 0xBEEF_0001;
        for byte in rgb.iter_mut().take(half) {
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
            "partial LSB embed should trip the windowed chi-square advisory: {}",
            lsb.detail
        );
    }

    #[test]
    fn text_with_some_trailing_whitespace_is_not_flagged() {
        // Under half the lines carry trailing whitespace — normal for some
        // source files; must not be flagged and must not "recover" a payload.
        let dir = tempdir().unwrap();
        let f = dir.path().join("code.txt");
        let mut body = String::new();
        for i in 0..40 {
            if i % 3 == 0 {
                body.push_str("trailing here \n"); // ~1/3 of lines
            } else {
                body.push_str("clean line\n");
            }
        }
        std::fs::write(&f, body).unwrap();
        let report = scan(&f);
        let ws = report
            .findings
            .iter()
            .find(|f| f.method == Method::Whitespace)
            .unwrap();
        assert!(!ws.suspicious, "should not recover a payload: {}", ws.detail);
        assert!(!ws.statistical_anomaly, "1/3 trailing should be normal");
    }

    #[test]
    fn whitespace_capacity_matches_embed() {
        // The reported capacity must be exactly the largest payload embed accepts.
        let dir = tempdir().unwrap();
        let src = dir.path().join("doc.txt");
        let out = dir.path().join("doc2.txt");
        let body: String = (0..512).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&src, &body).unwrap();
        let cap = whitespace::capacity_bytes(&body);
        assert!(cap > 0);
        // Exactly `cap` bytes must fit; `cap + 1` must not.
        let ok = vec![b'x'; cap as usize];
        embed(Method::Whitespace, &src, &out, &ok).unwrap();
        let too_big = vec![b'x'; cap as usize + 1];
        assert!(matches!(
            embed(Method::Whitespace, &src, &out, &too_big),
            Err(AnalysisError::Capacity { .. })
        ));
    }

    /// A minimal but structurally valid JPEG: SOI, an APP0 segment, a SOS whose
    /// entropy data includes FF00 byte-stuffing, then EOI. Exercises the JPEG
    /// marker walker without needing a JPEG encoder feature.
    fn make_jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8]; // SOI
        // APP0, length 4 (covers the length field + 2 payload bytes)
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        // SOS, length 4 + 2 header bytes
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x04, 0x00, 0x00]);
        // Entropy-coded data with a stuffed FF (FF 00 is data, not a marker).
        v.extend_from_slice(&[0x12, 0x34, 0xFF, 0x00, 0x56]);
        v.extend_from_slice(&[0xFF, 0xD9]); // EOI
        v
    }

    #[test]
    fn jpeg_walker_finds_eoi_and_appended_payload_round_trips() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("pic.jpg");
        let out = dir.path().join("pic_stego.jpg");
        std::fs::write(&src, make_jpeg()).unwrap();

        // Clean JPEG: no trailing data.
        let clean = scan(&src);
        let fa_clean = clean
            .findings
            .iter()
            .find(|f| f.method == Method::FormatAppend)
            .unwrap();
        assert!(!fa_clean.suspicious, "clean jpeg flagged: {}", fa_clean.detail);

        // A payload that itself contains an EOI marker (FF D9) must still be
        // recovered — the walker pins the real EOI before the appended data.
        let secret = b"\xFF\xD9 sneaky tail \xFF\xD9";
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
    }

    /// A minimal GIF87a: header, logical screen descriptor (no global color
    /// table), one image descriptor with a tiny image-data sub-block, trailer.
    fn make_gif() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"GIF87a");
        // Logical screen descriptor: width=1,height=1, packed=0 (no GCT), bg=0, ar=0
        v.extend_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
        // Image descriptor
        v.push(0x2C);
        v.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]); // 9 bytes, packed=0
        v.push(0x02); // LZW min code size
        v.extend_from_slice(&[0x02, 0x4C, 0x01]); // sub-block: len 2 + data
        v.push(0x00); // sub-block terminator
        v.push(0x3B); // trailer
        v
    }

    #[test]
    fn gif_walker_finds_trailer_and_appended_payload_round_trips() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("anim.gif");
        let out = dir.path().join("anim_stego.gif");
        std::fs::write(&src, make_gif()).unwrap();

        let clean = scan(&src);
        let fa_clean = clean
            .findings
            .iter()
            .find(|f| f.method == Method::FormatAppend)
            .unwrap();
        assert!(!fa_clean.suspicious, "clean gif flagged: {}", fa_clean.detail);

        // Payload containing a 0x3B (the trailer byte) must still round-trip.
        let secret = b"tail;with;semicolons";
        embed(Method::FormatAppend, &src, &out, secret).unwrap();
        let got = extract(Method::FormatAppend, &out).unwrap();
        assert_eq!(got, secret);
    }

    #[test]
    fn png_walker_pins_real_iend_past_appended_payload() {
        // Payload bytes that include "IEND" must not confuse the chunk walker.
        let dir = tempdir().unwrap();
        let src = dir.path().join("p.png");
        let out = dir.path().join("p_stego.png");
        make_png(&src, 16, 16);
        let secret = b"IEND is in here \x00\x00\x00\x00IEND and again";
        embed(Method::FormatAppend, &src, &out, secret).unwrap();
        let got = extract(Method::FormatAppend, &out).unwrap();
        assert_eq!(got, secret);
    }
}
