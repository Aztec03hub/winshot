//! Text legibility under downscaling, measured with the OCR engine built into
//! Windows (no extra dependency). Used by `winshot calibrate` (exhaustive search,
//! cached per display) and the default LLM mode (per image: shape-aware measurement).
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
/// ...and this many at the size itself (within 10% of it), so a lone short label just
/// below a real group (a 3-letter tab title under 15 px labels) cannot pin it either.
const MIN_AT_SIZE_CHARS: usize = 12;
/// Lines within this height ratio count as one text size.
const CLASS_SPAN: f32 = 1.2;
/// What a text line is made of. Claude's legibility threshold and OCR's height error
/// both depend on it, so one threshold for every line either wastes tokens on caps and
/// digits or loses lowercase (DESIGN §5d).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    /// has lowercase letters (prose, code, Mixed Case)
    Text,
    /// letters, none lowercase (SMALL-CAPS LABELS, headings)
    Caps,
    /// no letters (line numbers, clocks, counters)
    Digits,
}

impl Shape {
    pub fn of(words: &[String]) -> Shape {
        let letters: Vec<char> = words.iter().flat_map(|w| w.chars()).filter(|c| c.is_alphabetic()).collect();
        if letters.is_empty() {
            Shape::Digits
        } else if letters.iter().any(|c| c.is_lowercase()) {
            Shape::Text
        } else {
            Shape::Caps
        }
    }

    /// Rendered line height (px) Claude still reads after downscaling. Measured on Claude
    /// itself (claude-opus-5-5, 2026-09-23, scripts/claude-legibility.py): random words in
    /// Consolas and Segoe UI, light and dark themes. The worst font and theme set each
    /// value: every cell measured at or above it held at least 95% of full size. Text is
    /// 9 because Segoe UI lowercase and Mixed Case on dark were borderline at 8 (90-93%)
    /// and 95-98% at 9; caps held 98-100% at 7 and digits 98-100% at 6 in both themes.
    /// (Windows OCR needs ~10 px, which is why OCR is the ruler here, not the judge.)
    pub fn safe_px(self) -> f64 {
        match self {
            Shape::Text => 9.0,
            Shape::Caps => 7.0,
            Shape::Digits => 6.0,
        }
    }

    /// OCR box height / the height this shape's threshold is measured in, for a line of
    /// `words`: the LARGEST value that could apply, so the real height estimated from the
    /// box errs small and the scale errs large.
    ///
    /// - Text: see `text_box_ratio`.
    /// - Caps: OCR boxes plain capitals at 0.79-0.90 of their rendered height (measured on
    ///   Consolas and Segoe UI renders, DESIGN §5d); 0.90 is used. A caps line with a
    ///   bracket, comma, slash or accented letter can be boxed well past cap height, so
    ///   it gets 1.45 (a full-height box over 0.72-high capitals, with margin).
    /// - Digits: boxed at 0.96-1.00 of their height; 1.0 is used, 1.45 with such marks.
    pub fn ocr_ratio(self, words: &[String]) -> f64 {
        let stretched = words.iter().flat_map(|w| w.chars()).any(stretches_box);
        match self {
            Shape::Text => text_box_ratio(words),
            Shape::Caps if stretched => 1.45,
            Shape::Caps => 0.90,
            Shape::Digits if stretched => 1.45,
            Shape::Digits => 1.0,
        }
    }
}

/// Characters whose ink may reach both above and below the letters around them:
/// brackets, commas, slashes, j, and any non-ASCII letter (accents above, cedillas and
/// ogoneks below). Unknown marks count too, so every guess errs toward more resolution.
fn stretches_box(c: char) -> bool {
    const PLAIN: &str = "acemnorsuvwxz.:-=+~bdfhiklt!?&%#'\"*^`<>gpqy ";
    !(c.is_ascii_uppercase() || c.is_ascii_digit() || PLAIN.contains(c))
}

/// OCR boxes a word from its highest to its lowest ink, so a lowercase line with no
/// descenders (g p q y) is boxed short of its full height, and one with no ascenders
/// either shorter still. Measured with Windows OCR on Consolas and Segoe UI, light and
/// dark, as box / (ascender top to descender bottom): ascenders-only 0.769-0.778,
/// descenders-only 0.744-0.778, x-height-only 0.513-0.556. The largest is used. Only
/// letters and marks that stay inside the x-height band count as x-height; anything that
/// might stretch a box (see `stretches_box`) makes the line full height.
fn text_box_ratio(words: &[String]) -> f64 {
    const X: &str = "acemnorsuvwxz.:-=+~ ";
    const DESC: &str = "gpqy";
    let (mut up, mut down) = (false, false);
    for c in words.iter().flat_map(|w| w.chars()) {
        if stretches_box(c) {
            return 1.0;
        } else if DESC.contains(c) {
            down = true;
        } else if !X.contains(c) {
            up = true; // ascenders, i and t, capitals, digits, quotes: above x-height
        }
    }
    match (up, down) {
        (true, true) => 1.0,
        (true, false) | (false, true) => 0.778,
        (false, false) => 0.556,
    }
}

/// One OCR'd text line: height in px (tallest word box) and its words.
#[derive(Clone)]
pub struct Line {
    pub h: f32,
    pub words: Vec<String>,
    /// Bounding box in the measured image's pixels: x, y, w, h.
    pub rect: (f32, f32, f32, f32),
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
    /// The letter shape of the text that set the scale (`text_scale` only).
    pub shape: Option<Shape>,
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
    Ok(Verdict { scale: hi, small, shape: None, floor, steps })
}

/// Every OCR line of `f`, for `--dump-text` (analysis): height, shape, position, words.
pub fn dump(f: &Frame) -> Result<()> {
    let lines = ocr_lines(&engine()?, f)?;
    for l in &lines {
        let (x, y, w, h) = l.rect;
        let shape = Shape::of(&l.words);
        let real = l.h as f64 / shape.ocr_ratio(&l.words);
        println!("{:>5.1} {:>5.1} {:<6} {:>5.0},{:<5.0} {:>5.0}x{:<4.0} {}", l.h, real, format!("{:?}", shape), x, y, w, h, l.words.join(" "));
    }
    let groups = shape_groups(&lines);
    let bind = groups.iter().map(|g| g.0).fold(0.0, f64::max);
    for (need, small, shape) in &groups {
        println!(
            "## group {:?}: smallest real {:.1} px ({} lines, e.g. \"{}\"), threshold {} px, needs x{:.3}{}",
            shape,
            small.h,
            small.lines,
            small.words.iter().take(6).cloned().collect::<Vec<_>>().join(" "),
            shape.safe_px(),
            need,
            if *need == bind { "  <- sets the scale" } else { "" }
        );
    }
    Ok(())
}

/// The default mode's text-safe scale for THIS image. Lines are grouped by letter shape;
/// in each group the smallest text size with real content needs the scale that puts
/// its (estimated rendered) height at that shape's threshold, and the most demanding
/// group wins. OCR measures the text at full resolution; it does not judge the result,
/// because it is stricter than Claude. None when there is too little text to measure:
/// the caller falls back to calibrated or 1/DPI.
pub fn text_scale(f: &Frame) -> Result<Option<Verdict>> {
    let engine = engine()?;
    let lines = ocr_lines(&engine, f)?;
    Ok(shape_scale(&lines).map(|(s, small, shape)| Verdict { scale: if s >= IDENTITY { 1.0 } else { s }, small, shape: Some(shape), floor: 1.0, steps: vec![] }))
}

/// The scale each letter-shape group needs, and the most demanding group.
/// `SmallText::h` is the estimated REAL line height (in the shape's threshold units):
/// every line's OCR box is converted first, from its own letters, and only then grouped,
/// so a short x-height-only label cannot make taller text around it look bigger.
pub fn shape_scale(lines: &[Line]) -> Option<(f64, SmallText, Shape)> {
    shape_groups(lines).into_iter().fold(None, |best, (need, small, shape)| match best {
        Some((b, _, _)) if b >= need => best,
        _ => Some((need, small, shape)),
    })
}

/// Every letter-shape group that has real text: the scale it needs, its smallest text
/// (real height), and the shape. `shape_scale` takes the most demanding.
pub fn shape_groups(lines: &[Line]) -> Vec<(f64, SmallText, Shape)> {
    let mut out = Vec::new();
    for shape in [Shape::Text, Shape::Caps, Shape::Digits] {
        let group: Vec<Line> = lines
            .iter()
            .filter(|l| Shape::of(&l.words) == shape)
            .map(|l| Line { h: (l.h as f64 / shape.ocr_ratio(&l.words)) as f32, ..l.clone() })
            .collect();
        let Some(mut small) = smallest_text(&group) else { continue };
        small.all_lines = lines.len();
        let need = (shape.safe_px() / small.h as f64).clamp(MIN_SCALE, 1.0);
        out.push((need, small, shape));
    }
    out
}

/// The smallest line height h whose lines within [h, 1.2h] hold enough characters
/// to be real text, with those lines' words.
pub fn smallest_text(lines: &[Line]) -> Option<SmallText> {
    let mut hs: Vec<f32> = lines.iter().map(|l| l.h).collect();
    hs.sort_by(f32::total_cmp);
    let chars = |ls: &[&Line]| -> usize { ls.iter().flat_map(|l| &l.words).map(|w| w.chars().filter(|c| c.is_alphanumeric()).count()).sum() };
    hs.into_iter().find_map(|h| {
        let at_size: Vec<&Line> = lines.iter().filter(|l| l.h >= h && l.h <= h * 1.1).collect();
        if chars(&at_size) < MIN_AT_SIZE_CHARS {
            return None;
        }
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
        let (ox, oy) = if wide { (a as f32, 0.0) } else { (0.0, a as f32) };
        out.extend(ocr_band(engine, &if wide { tile::transpose(&band) } else { band })?.into_iter().map(|mut l| {
            l.rect.0 += ox;
            l.rect.1 += oy;
            l
        }));
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
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for w in line.Words()? {
            let r = w.BoundingRect()?;
            h = h.max(r.Height);
            (x0, y0, x1, y1) = (x0.min(r.X), y0.min(r.Y), x1.max(r.X + r.Width), y1.max(r.Y + r.Height));
            words.push(w.Text()?.to_string());
        }
        if !words.is_empty() {
            out.push(Line { h, words, rect: (x0, y0, x1 - x0, y1 - y0) });
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
        Line { h, words: text.split(' ').map(String::from).collect(), rect: (0.0, 0.0, 0.0, 0.0) }
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
    fn shape_classifies_lines() {
        let w = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(Shape::of(&w("INSERTIONS DELETIONS MOVED")), Shape::Caps);
        assert_eq!(Shape::of(&w("+35 INSERTIONS")), Shape::Caps);
        assert_eq!(Shape::of(&w("54 55 56 4:10")), Shape::Digits);
        assert_eq!(Shape::of(&w("return this.get(key)")), Shape::Text);
        assert_eq!(Shape::of(&w("Save As PDF")), Shape::Text);
    }

    #[test]
    fn caps_labels_need_less_scale_than_lowercase_of_the_same_box_height() {
        // 13 px caps labels (OCR boxes caps at cap height) and 22 px lowercase body text.
        let mut lines = vec![line(13.0, "INSERTIONS DELETIONS MODIFIED MOVED ADDED")];
        lines.extend((0..10).map(|_| line(22.0, "const rule = line.split(the) other, words")));
        let (s, small, shape) = shape_scale(&lines).unwrap();
        // caps: 13 px box / 0.90 = 14.4 px real, 7 / 14.4 = 0.485; text: 9 / 22 = 0.409.
        assert_eq!(shape, Shape::Caps);
        assert!((small.h - 13.0 / 0.9).abs() < 1e-3, "{}", small.h);
        assert!((s - 7.0 * 0.9 / 13.0).abs() < 1e-6, "{s}");
        // The same 13 px as lowercase needs 9/13 = 0.692.
        // Lowercase with ascenders but no descenders: boxed at 0.778 of its real height.
        let lower: Vec<Line> = (0..3).map(|_| line(13.0, "insertions deletions modified moved added")).collect();
        let (s2, _, shape2) = shape_scale(&lower).unwrap();
        assert_eq!(shape2, Shape::Text);
        assert!((s2 - 9.0 * 0.778 / 13.0).abs() < 1e-6, "{s2}");
    }

    #[test]
    fn letter_class_sets_the_box_ratio() {
        let w = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(text_box_ratio(&w("height gauge")), 1.0);
        assert_eq!(text_box_ratio(&w("added removed")), 0.778);
        assert_eq!(text_box_ratio(&w("mono excess")), 0.556);
        assert_eq!(text_box_ratio(&w("move on, now")), 1.0, "a comma may reach below the baseline");
        assert_eq!(text_box_ratio(&w("return")), 0.778, "t reaches above x-height");
        assert_eq!(text_box_ratio(&w("name = \"raw\"")), 0.778, "quotes reach above x-height");
        assert_eq!(text_box_ratio(&w("join main")), 1.0, "j drops below the baseline");
        assert_eq!(text_box_ratio(&w("façade")), 1.0, "a cedilla drops below");
        assert_eq!(Shape::Caps.ocr_ratio(&w("FILES (3), QUEUED")), 1.45);
        assert_eq!(Shape::Caps.ocr_ratio(&w("FILES QUEUED")), 0.90);
        assert_eq!(Shape::Digits.ocr_ratio(&w("1,234")), 1.45);
    }

    #[test]
    fn short_x_height_labels_do_not_hide_taller_text() {
        // Review scenario: 12 px boxes of x-height-only words (real ~21.6 px) next to
        // 14 px full-height lines. The 14 px lines are the real smallest text.
        let mut lines: Vec<Line> = (0..3).map(|_| line(12.0, "someone ran over some rows")).collect();
        lines.extend((0..10).map(|_| line(14.0, "height gauge filtering")));
        let (s, small, shape) = shape_scale(&lines).unwrap();
        assert_eq!(shape, Shape::Text);
        assert!((small.h - 14.0).abs() < 1e-3, "{}", small.h);
        assert!((s - 9.0 / 14.0).abs() < 1e-6, "{s}");
    }

    #[test]
    fn a_lone_short_label_does_not_pin_its_group() {
        // A 3-letter tab title at 13 px just below a real group of 15 px labels.
        let lines = vec![line(13.0, "DEV"), line(15.0, "INSERTIONS DELETIONS MODIFIED MOVED")];
        let (_, small, shape) = shape_scale(&lines).unwrap();
        assert_eq!(shape, Shape::Caps);
        assert!((small.h - 15.0 / 0.9).abs() < 1e-3);
        // With real content at 13 px, 13 px sets it.
        let lines = vec![line(13.0, "STATUS READY BRANCH MAIN"), line(15.0, "INSERTIONS DELETIONS MODIFIED MOVED")];
        assert!((shape_scale(&lines).unwrap().1.h - 13.0 / 0.9).abs() < 1e-3);
    }

    #[test]
    fn no_text_gives_none() {
        assert!(smallest_text(&[line(9.0, "ok")]).is_none());
    }
}
