//! Text legibility under downscaling, measured with the OCR engine built into
//! Windows (no extra dependency). Used by `winshot calibrate` (exhaustive search,
//! cached per display) and `--for-claude` (per image: predict, then verify).
//!
//! Screens mix text sizes, so nothing here scores the whole screen: an average lets
//! a minority size (a status bar, footnotes, a sidebar) vanish while the score stays
//! high. Every verdict is on the SMALLEST text size that carries real content.

use crate::capture::Frame;
use crate::encode::{resize, Filter};
use crate::tile;
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

/// Accept a scale while OCR still reads this fraction of the smallest text's words
/// that it reads at ~identity. Relative, not absolute: Windows OCR re-reads only
/// 83-98% of words after an invisible x0.99 round trip, so an absolute 99.5% is
/// unreachable (DESIGN §5). The tuning knob: raise it for a stricter (larger) scale.
pub const RETAIN: f64 = 0.90;
const IDENTITY: f64 = 0.99;
const MIN_SCALE: f64 = 0.25;
/// Smallest text size worth protecting must hold this many letters/digits in total,
/// so an icon misread as "e" or a lone badge digit cannot pin the scale.
const MIN_CLASS_CHARS: usize = 24;
/// Lines within this height ratio count as one text size.
const CLASS_SPAN: f32 = 1.2;
/// Line height in px (as OCR measures it) that Claude still reads after downscaling.
/// Measured on Claude itself (claude-opus-5-5, 2026-09-23, scripts/claude-legibility.py):
/// random words, Consolas and Segoe UI, mixed case / CAPS / digits; reading held at
/// least 90% of full size down to 6-7 px in the worst shapes, and 93% everywhere at 8.
/// OCR measures a line at 0.79-1.00x its rendered height, so applying this to OCR
/// heights errs only toward more resolution. (Windows OCR itself needs ~10 px:
/// scripts/check-legibility.py sweep; that is why OCR is the ruler here, not the judge.)
pub const SAFE_LINE_PX: f64 = 8.0;

/// One OCR'd text line: height in px (tallest word box) and its words.
pub struct Line {
    pub h: f32,
    pub words: Vec<String>,
}

/// The smallest text size with real content.
pub struct SmallText {
    pub h: f32,
    pub words: Vec<String>,
    pub lines: usize,
    pub all_lines: usize,
}

pub struct Verdict {
    pub scale: f64,
    pub small: SmallText,
    pub floor: f64,              // smallest text's recall at x0.99: the engine's own noise ceiling
    pub steps: Vec<(f64, f64)>, // (scale, smallest text's recall)
}

fn engine() -> Result<OcrEngine> {
    Ok(OcrEngine::TryCreateFromUserProfileLanguages()?)
}

/// Recall of `small`'s words after a round trip through scale s. The candidate is
/// scaled back up to the reference size before OCR, so the engine always sees the
/// same geometry and only the glyph information the downscale destroyed varies.
fn recall(engine: &OcrEngine, f: &Frame, small: &SmallText, s: f64) -> Result<f64> {
    let back = resize(&tile::scaled(f, s, Filter::Lanczos3)?, f.w, f.h, Filter::Lanczos3)?;
    let seen: Vec<String> = ocr_lines(engine, &back)?.into_iter().flat_map(|l| l.words).collect();
    Ok(agreement(&seen.join(" "), &small.words.join(" ")))
}

fn measure(engine: &OcrEngine, f: &Frame) -> Result<Option<SmallText>> {
    let lines = ocr_lines(engine, f)?;
    Ok(smallest_text(&lines))
}

/// `winshot calibrate`: exhaustive binary search on the smallest text's recall.
pub fn calibrate(f: &Frame) -> Result<Verdict> {
    let engine = engine()?;
    let Some(small) = measure(&engine, f)? else {
        bail!("not enough text found; calibrate on a text-dense screen (a terminal or an editor)");
    };
    let mut steps = Vec::new();
    let mut ok = |s: f64, floor: f64| -> Result<bool> {
        let r = recall(&engine, f, &small, s)?;
        steps.push((s, r));
        Ok(r >= floor * RETAIN)
    };
    let floor = recall(&engine, f, &small, IDENTITY)?;
    if floor < 0.5 {
        bail!("OCR re-reads only {:.0}% of the smallest text even at x{IDENTITY}; too unreliable to calibrate on", floor * 100.0);
    }
    // Assumes readability is roughly monotone in scale (it is, to within OCR noise,
    // on the captures in DESIGN §5).
    let (mut lo, mut hi) = (MIN_SCALE, IDENTITY);
    if ok(lo, floor)? {
        hi = lo;
    } else {
        for _ in 0..5 {
            let mid = (lo + hi) / 2.0;
            if ok(mid, floor)? {
                hi = mid;
            } else {
                lo = mid;
            }
        }
    }
    Ok(Verdict { scale: hi, small, floor, steps })
}

/// `--for-claude`: the text-safe scale for THIS image: the scale that puts its
/// smallest text at `SAFE_LINE_PX`, the height measured on Claude itself. OCR measures
/// the text at full resolution; it does not judge the result, because it is stricter
/// than Claude (it would push every scale back up to its own ~10 px). None when there
/// is too little text to measure: the caller falls back to calibrated or 1/DPI.
pub fn text_scale(f: &Frame) -> Result<Option<Verdict>> {
    let engine = engine()?;
    let Some(small) = measure(&engine, f)? else { return Ok(None) };
    let s = (SAFE_LINE_PX / small.h as f64).clamp(MIN_SCALE, 1.0);
    Ok(Some(Verdict { scale: if s >= IDENTITY { 1.0 } else { s }, small, floor: 1.0, steps: vec![] }))
}

/// The smallest line height h whose lines within [h, 1.2h] hold enough characters
/// to be real text, with those lines' words.
pub fn smallest_text(lines: &[Line]) -> Option<SmallText> {
    let mut hs: Vec<f32> = lines.iter().map(|l| l.h).collect();
    hs.sort_by(f32::total_cmp);
    hs.into_iter().find_map(|h| {
        let class: Vec<&Line> = lines.iter().filter(|l| l.h >= h && l.h <= h * CLASS_SPAN).collect();
        let words: Vec<String> = class.iter().flat_map(|l| l.words.iter().cloned()).collect();
        let chars: usize = words.iter().map(|w| w.chars().filter(|c| c.is_alphanumeric()).count()).sum();
        (chars >= MIN_CLASS_CHARS).then_some(SmallText { h, words, lines: class.len(), all_lines: lines.len() })
    })
}

/// OCR, in bands cut at blank rows/columns when the image exceeds the engine's
/// maximum dimension (tall pages), so no band boundary truncates a line's height.
pub fn ocr_lines(engine: &OcrEngine, f: &Frame) -> Result<Vec<Line>> {
    let max = OcrEngine::MaxImageDimension()?;
    if f.w <= max && f.h <= max {
        return ocr_band(engine, f);
    }
    let wide = f.w > f.h;
    let g = if wide { tile::transpose(f) } else { f.clone() };
    let mut out = Vec::new();
    for (a, b) in tile::spans(&g, max).0 {
        let band = g.crop(0, a, g.w, b - a);
        out.extend(ocr_band(engine, &if wide { tile::transpose(&band) } else { band })?);
    }
    Ok(out)
}

fn ocr_band(engine: &OcrEngine, f: &Frame) -> Result<Vec<Line>> {
    let max = OcrEngine::MaxImageDimension()?;
    // ponytail: the short edge beyond the OCR limit is ignored; only a > 10000 px wide AND tall image hits it.
    let f = if f.w > max || f.h > max { &f.crop(0, 0, f.w.min(max), f.h.min(max)) } else { f };
    let bgra: Vec<u8> = f.rgb.chunks_exact(3).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    let dw = DataWriter::new()?;
    dw.WriteBytes(&bgra)?;
    let bmp = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(&dw.DetachBuffer()?, BitmapPixelFormat::Bgra8, f.w as i32, f.h as i32, BitmapAlphaMode::Ignore)?;
    let mut out = Vec::new();
    for line in engine.RecognizeAsync(&bmp)?.join()?.Lines()? {
        let (mut h, mut words) = (0f32, Vec::new());
        for w in line.Words()? {
            h = h.max(w.BoundingRect()?.Height);
            words.push(w.Text()?.to_string());
        }
        if !words.is_empty() {
            out.push(Line { h, words });
        }
    }
    Ok(out)
}

/// Fraction of the reference's words the candidate read back exactly, as a
/// multiset (order-insensitive). Not whole-string edit distance: a small rescale
/// makes the OCR engine re-segment and re-order lines and columns, and edit
/// distance then scores reading order instead of legibility (measured, DESIGN §5).
pub fn agreement(candidate: &str, reference: &str) -> f64 {
    let mut want: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for w in reference.split_whitespace() {
        *want.entry(w).or_default() += 1;
    }
    let total: usize = want.values().sum();
    if total == 0 {
        return 1.0;
    }
    let mut hit = 0;
    for w in candidate.split_whitespace() {
        if let Some(n) = want.get_mut(w).filter(|n| **n > 0) {
            *n -= 1;
            hit += 1;
        }
    }
    hit as f64 / total as f64
}

fn cache_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into());
    base.join("winshot").join("calibration.toml")
}

/// Per-display safe scale, keyed by display name (DISPLAY1, ...).
pub fn load() -> BTreeMap<String, f64> {
    std::fs::read_to_string(cache_path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

pub fn store(display: &str, scale: f64) -> Result<PathBuf> {
    let mut all = load();
    all.insert(display.to_string(), scale);
    let p = cache_path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, toml::to_string(&all)?)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agreement_is_order_insensitive_word_recall() {
        assert_eq!(agreement("world\nhello", "hello  world"), 1.0);
        assert_eq!(agreement("a b c x", "a b c d"), 0.75);
        assert_eq!(agreement("a a", "a a a a"), 0.5); // multiset, not set
        assert_eq!(agreement("", "abcd"), 0.0);
    }

    fn line(h: f32, text: &str) -> Line {
        Line { h, words: text.split(' ').map(String::from).collect() }
    }

    #[test]
    fn smallest_text_is_the_minority_size_not_the_average() {
        let mut lines: Vec<Line> = (0..40).map(|_| line(30.0, "body text of the document here")).collect();
        lines.push(line(12.0, "status: 3 errors, 14 warnings, branch main"));
        let s = smallest_text(&lines).unwrap();
        assert_eq!(s.h, 12.0, "one small status line among 40 body lines still sets the floor");
        assert_eq!(s.lines, 1);
    }

    #[test]
    fn stray_specks_do_not_pin_the_scale() {
        let mut lines: Vec<Line> = (0..10).map(|_| line(30.0, "body text of the document here")).collect();
        lines.push(line(6.0, "e")); // an icon misread as a letter
        lines.push(line(7.0, "3")); // a badge digit
        assert_eq!(smallest_text(&lines).unwrap().h, 30.0);
    }

    #[test]
    fn no_text_gives_none() {
        assert!(smallest_text(&[line(9.0, "ok")]).is_none());
    }
}
