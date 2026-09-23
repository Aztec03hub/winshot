//! Default (LLM) mode geometry: fit Claude's vision limits without shrinking text past
//! legibility. The text decides the scale; the number of images absorbs the length.
//! (The scale itself comes from `legible::text_scale`, measured on the image.)

use crate::capture::Frame;
use crate::encode::{resize, Filter};
use anyhow::Result;

/// Claude rejects images over 8000 px on a side.
pub const MAX_EDGE: u32 = 8000;
/// A single image is allowed to dip this far under the text-safe scale, so a normal
/// screenshot (3840 wide -> 2000 is x0.521 vs a measured safe ~0.53) stays one image.
const SLACK: f64 = 0.95;
/// Claude sees images in 28x28-pixel patches, one visual token each.
const PATCH: u32 = 28;

/// Claude's image resolution tiers (docs: vision, "Resolution and token cost").
/// Anything over either limit is silently downscaled by the API.
#[derive(Clone, Copy, PartialEq, Debug, clap::ValueEnum)]
pub enum Tier {
    /// Claude 4.7 and later: 2576 px long edge, 4784 visual tokens
    High,
    /// older models (e.g. Haiku 4.5): 1568 px, 1568 visual tokens
    Standard,
}

impl Tier {
    pub fn max_edge(self) -> u32 {
        match self {
            Tier::High => 2576,
            Tier::Standard => 1568,
        }
    }
    pub fn max_tokens(self) -> u32 {
        match self {
            Tier::High => 4784,
            Tier::Standard => 1568,
        }
    }
}

/// Exact visual-token cost of a w x h image: one per 28x28 patch, rounded up.
pub fn tokens(w: u32, h: u32) -> u32 {
    w.div_ceil(PATCH) * h.div_ceil(PATCH)
}

/// Input tokens an image block really bills: its patches plus a fixed per-image
/// overhead. Measured with count_tokens on claude-opus-5-5 (2026-09-23): exactly +3
/// on all 9 images tried, 304x2000 to 2000x1250 (scripts/count-tokens.py).
pub fn billed(w: u32, h: u32) -> u32 {
    tokens(w, h) + 3
}

/// The size the API resizes an image to: a port of the docs' reference `resized_size` (vision-coordinates, "How Claude resizes and pads
/// images"), including its round-half-to-even on the short edge.
pub fn api_size(w: u32, h: u32, max_edge: u32, max_tokens: u32) -> (u32, u32) {
    let fits = |w: u32, h: u32| w.div_ceil(PATCH) * PATCH <= max_edge && h.div_ceil(PATCH) * PATCH <= max_edge && tokens(w, h) <= max_tokens;
    if fits(w, h) {
        return (w, h);
    }
    if h > w {
        let (rh, rw) = api_size(h, w, max_edge, max_tokens);
        return (rw, rh);
    }
    let aspect = w as f64 / h as f64;
    let short = |long: u32| ((long as f64 / aspect).round_ties_even() as u32).max(1);
    let (mut lo, mut hi) = (1, w); // lo always fits, hi never does
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if fits(mid, short(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo, short(lo))
}

/// What one image of w x h becomes on the way to Claude: the long-edge cap (raw
/// pixels, as Claude Code applies its 2000 px), then exactly the API's own rule.
pub fn fit_size(w: u32, h: u32, long_edge: u32, tier: Tier) -> (u32, u32) {
    let s = (long_edge as f64 / w.max(h) as f64).min(1.0);
    let (w0, h0) = if s < 1.0 { (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1)) } else { (w, h) };
    api_size(w0, h0, tier.max_edge(), tier.max_tokens())
}

/// The largest scale >= s that adds no patches on either axis: tokens are paid per
/// whole 28 px patch, so the partial patch's resolution is free.
fn patch_aligned(w: u32, h: u32, s: f64) -> f64 {
    let up = |d: u32| (((d as f64 * s).round() as u32).max(1).div_ceil(PATCH) * PATCH) as f64 / d as f64;
    up(w).min(up(h)).clamp(s, 1.0)
}

pub struct Plan {
    pub scale: f64,
    pub tiles: Vec<Frame>,
    /// Per seam: true when it fell in a blank row/column, false when it had to overlap.
    pub seams: Vec<bool>,
    /// The short edge alone forced a scale below the text-safe one.
    pub short_edge_limited: bool,
    /// The edge limit actually applied: min(--long-edge, the tier's padded edge).
    pub edge: u32,
}

pub fn scaled(f: &Frame, s: f64, filter: Filter) -> Result<Frame> {
    if s >= 1.0 {
        return Ok(f.clone());
    }
    resize(f, ((f.w as f64 * s).round() as u32).max(1), ((f.h as f64 * s).round() as u32).max(1), filter)
}

/// Fit `f` into images nothing downstream will resize: long edge <= `long_edge`
/// (Claude Code's own cap is 2000 px on the raw edge) and within the API tier's
/// patch-padded edge and visual-token limits. Never scales below `text_scale`
/// except when the *short* edge alone does not fit. `lean` also shrinks an image
/// that already fits down to `text_scale`: fewest tokens with the text still legible.
pub fn plan(f: &Frame, text_scale: f64, long_edge: u32, tier: Tier, lean: bool, filter: Filter) -> Result<Plan> {
    let cap = tier.max_tokens();
    let edge = long_edge.min(tier.max_edge() / PATCH * PATCH);
    let (fw, fh) = fit_size(f.w, f.h, long_edge, tier);
    let fit = fw as f64 / f.w as f64;
    if fit >= text_scale.min(1.0) * SLACK {
        if lean && text_scale < fit {
            let s = patch_aligned(f.w, f.h, text_scale).min(fit);
            let one = resize(f, ((f.w as f64 * s).round() as u32).max(1), ((f.h as f64 * s).round() as u32).max(1), filter)?;
            return Ok(Plan { scale: s, tiles: vec![one], seams: vec![], short_edge_limited: false, edge });
        }
        let one = if (fw, fh) == (f.w, f.h) { f.clone() } else { resize(f, fw, fh, filter)? };
        return Ok(Plan { scale: fit, tiles: vec![one], seams: vec![], short_edge_limited: false, edge });
    }
    // Too long to fit at a readable scale: keep the text scale and split the long axis.
    // ponytail: splits one axis only; an image too big on BOTH axes shrinks its short
    // edge to fit and says so, a 2-D grid would avoid that.
    let (long, short) = (f.w.max(f.h), f.w.min(f.h));
    let max_short = edge;
    // Tokens are paid per whole patch, so round the short side UP to a patch boundary:
    // the extra resolution is free.
    let want = ((short as f64 * text_scale).round() as u32).max(1).div_ceil(PATCH) * PATCH;
    let s_short = want.min(max_short).min(short);
    let s = s_short as f64 / short as f64;
    let s_long = ((long as f64 * s).round() as u32).max(1);
    let img = if s >= 1.0 {
        f.clone()
    } else if f.w >= f.h {
        resize(f, s_long, s_short, filter)?
    } else {
        resize(f, s_short, s_long, filter)?
    };
    let wide = img.w > img.h;
    let img = if wide { transpose(&img) } else { img };
    // Longest tile that stays inside both the edge and the visual-token cap.
    let lmax = edge.min(cap / img.w.div_ceil(PATCH) * PATCH);
    let (spans, seams) = spans(&img, lmax);
    let tiles = spans
        .iter()
        .map(|&(a, b)| {
            let t = img.crop(0, a, img.w, b - a);
            if wide {
                transpose(&t)
            } else {
                t
            }
        })
        .collect();
    Ok(Plan { scale: s, tiles, seams, short_edge_limited: s < text_scale * 0.999, edge })
}

/// Crop uniform margins: rows and columns at the edges that match the corner
/// colour carry no content and still cost a visual token per 28 px patch.
/// Returns the trimmed frame and the kept rect (in `f`'s coordinates).
pub fn trim(f: &Frame) -> (Frame, (u32, u32, u32, u32)) {
    let bg = [f.rgb[0], f.rgb[1], f.rgb[2]];
    // Within a few levels of the background: tolerates dithering and gradients' first steps.
    let plain = |x: u32, y: u32| {
        let i = (y as usize * f.w as usize + x as usize) * 3;
        f.rgb[i..i + 3].iter().zip(bg).map(|(a, b)| a.abs_diff(b) as u32).sum::<u32>() <= 12
    };
    let row_plain = |y: u32| (0..f.w).all(|x| plain(x, y));
    let col_plain = |x: u32, y0: u32, y1: u32| (y0..y1).all(|y| plain(x, y));
    let top = (0..f.h).find(|&y| !row_plain(y));
    let Some(top) = top else { return (f.clone(), (0, 0, f.w, f.h)) }; // entirely plain: leave it
    let bottom = (0..f.h).rev().find(|&y| !row_plain(y)).unwrap() + 1;
    let left = (0..f.w).find(|&x| !col_plain(x, top, bottom)).unwrap();
    let right = (0..f.w).rev().find(|&x| !col_plain(x, top, bottom)).unwrap() + 1;
    // Keep a little breathing room so glyphs are not flush with the edge.
    const PAD: u32 = 6;
    let (x0, y0) = (left.saturating_sub(PAD), top.saturating_sub(PAD));
    let (x1, y1) = ((right + PAD).min(f.w), (bottom + PAD).min(f.h));
    (f.crop(x0, y0, x1 - x0, y1 - y0), (x0, y0, x1 - x0, y1 - y0))
}

/// Horizontal detail in row y: sum of |neighbour differences|. A blank row between
/// text lines is ~0 even on a vertical gradient background.
fn row_energy(f: &Frame, y: u32) -> u64 {
    let row = &f.rgb[y as usize * f.w as usize * 3..][..f.w as usize * 3];
    let px = row.chunks_exact(3);
    px.clone().zip(px.skip(1)).map(|(a, b)| a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum::<u64>()).sum()
}

/// Split rows [0, h) into balanced spans of <= max rows. Each cut goes in the blank
/// row nearest the balanced cut point (within a quarter span), so no text line is
/// sliced and the last tile is not a sliver; when there is no blank row there (a
/// photo, dense code, text running across the cut), cut at the balanced point and
/// overlap the next span, so a sliced line appears whole in one of the two.
pub fn spans(f: &Frame, max: u32) -> (Vec<(u32, u32)>, Vec<bool>) {
    // ~2 abs-diff units per pixel: tolerates a divider line or scrollbar crossing a
    // blank row, rejects any row through glyphs (measured ~20+ per pixel on text).
    let blank = 2 * f.w as u64;
    let overlap = (max / 16).max(16);
    let (mut out, mut seams, mut start) = (Vec::new(), Vec::new(), 0u32);
    while f.h - start > max {
        let left = f.h - start;
        let ideal = start + left / left.div_ceil(max);
        let (lo, hi) = (ideal - max / 4, (ideal + max / 4).min(start + max));
        let best = (lo..=hi).filter(|&y| row_energy(f, y) <= blank).min_by_key(|&y| y.abs_diff(ideal));
        match best {
            Some(y) => {
                out.push((start, y));
                seams.push(true);
                start = y;
            }
            None => {
                out.push((start, ideal));
                seams.push(false);
                start = ideal - overlap;
            }
        }
    }
    out.push((start, f.h));
    (out, seams)
}

pub fn transpose(f: &Frame) -> Frame {
    let mut out = Frame::black(f.h, f.w);
    for y in 0..f.h as usize {
        for x in 0..f.w as usize {
            let s = (y * f.w as usize + x) * 3;
            let d = (x * f.h as usize + y) * 3;
            out.rgb[d..d + 3].copy_from_slice(&f.rgb[s..s + 3]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// w x h dark page with 20-row "text lines" (noisy stripes) every 32 rows.
    fn page(w: u32, h: u32) -> Frame {
        let mut f = Frame::black(w, h);
        for y in 0..h {
            if y % 32 < 20 {
                for x in 0..w {
                    let i = ((y * w + x) * 3) as usize;
                    let v = if (x * 7 + y * 3) % 5 < 2 { 230 } else { 20 };
                    f.rgb[i..i + 3].copy_from_slice(&[v, v, v]);
                }
            }
        }
        f
    }

    fn is_text_row(y: u32) -> bool {
        y % 32 < 20
    }

    #[test]
    fn cuts_land_between_lines_and_tiles_fit() {
        let f = page(300, 5000);
        let (spans, seams) = spans(&f, 1000);
        assert!(seams.iter().all(|&b| b), "every seam should find a blank row");
        for (i, &(a, b)) in spans.iter().enumerate() {
            assert!(b - a <= 1000, "span {i} is {} rows", b - a);
            if i > 0 {
                assert!(!is_text_row(a), "cut at row {a} slices a text line");
            }
        }
        assert_eq!(spans.last().unwrap().1, 5000);
        assert!(spans.windows(2).all(|p| p[0].1 == p[1].0), "blank cuts need no overlap");
        assert!(spans.iter().all(|&(a, b)| b - a >= 500), "balanced: no sliver tiles {spans:?}");
    }

    #[test]
    fn two_tiles_split_near_the_middle() {
        let (spans, _) = spans(&page(300, 2600), 2000);
        assert_eq!(spans.len(), 2);
        let cut = spans[0].1 as i64;
        assert!((cut - 1300).abs() <= 32, "cut at {cut}, want the blank row nearest 1300");
    }

    #[test]
    fn no_blank_rows_means_overlap() {
        let mut f = Frame::black(64, 3000);
        for (i, b) in f.rgb.iter_mut().enumerate() {
            *b = if (i / 3) % 2 == 0 { 255 } else { 0 };
        }
        let (spans, seams) = spans(&f, 1000);
        assert!(seams.iter().all(|&b| !b));
        assert!(spans.windows(2).all(|p| p[1].0 < p[0].1), "spans must overlap");
        assert!(spans.iter().all(|&(a, b)| b - a <= 1000));
    }

    #[test]
    fn tall_image_keeps_text_scale_and_tiles() {
        let f = page(1280, 12000);
        let p = plan(&f, 0.5, 2000, Tier::High, false, Filter::Box).unwrap();
        assert!(p.scale >= 0.5, "must not shrink below the text scale");
        assert_eq!(p.tiles[0].w % 28, 0, "short side grown to a whole patch (free resolution)");
        // 6000 rows at <= 2000 per tile; blank-row cuts land a little early, so 3 or 4.
        assert!((3..=4).contains(&p.tiles.len()), "{} tiles", p.tiles.len());
        assert!(p.tiles.iter().all(|t| t.w == 644 && t.h <= 2000 && tokens(t.w, t.h) <= 4784));
    }

    #[test]
    fn wide_image_tiles_along_width() {
        let f = transpose(&page(600, 9000));
        let p = plan(&f, 1.0, 2000, Tier::High, false, Filter::Box).unwrap();
        assert!(p.tiles.len() >= 5);
        assert!(p.tiles.iter().all(|t| t.h == 600 && t.w <= 2000));
    }

    #[test]
    fn normal_screenshot_is_one_image() {
        let f = page(3840, 2400);
        let p = plan(&f, 0.527, 2000, Tier::High, false, Filter::Box).unwrap();
        assert_eq!(p.tiles.len(), 1, "x0.521 is within slack of x0.527");
        assert_eq!((p.tiles[0].w, p.tiles[0].h), (2000, 1250));
    }

    #[test]
    fn lean_shrinks_a_fitting_image_to_the_text_scale_on_patch_boundaries() {
        let f = page(1920, 1080);
        let keep = plan(&f, 0.6, 2000, Tier::High, false, Filter::Box).unwrap();
        assert_eq!((keep.tiles[0].w, keep.tiles[0].h), (1920, 1080), "fits: kept whole");
        let lean = plan(&f, 0.6, 2000, Tier::High, true, Filter::Box).unwrap();
        let t = &lean.tiles[0];
        assert!(lean.scale >= 0.6 && lean.scale < 0.62, "x{}", lean.scale);
        assert_eq!(tokens(t.w, t.h), tokens((1920.0 * 0.6f64).round() as u32, (1080.0 * 0.6f64).round() as u32), "patch rounding adds no tokens");
        assert!(tokens(t.w, t.h) < tokens(1920, 1080) / 2);
    }

    #[test]
    fn api_size_matches_the_docs_examples() {
        // Every example in the vision docs' tables and the A4 example.
        assert_eq!(api_size(1075, 1520, 1568, 1568), (924, 1307));
        assert_eq!(api_size(1920, 1080, 1568, 1568), (1456, 819));
        // The docs' table says 1269x952; the docs' own reference code says 1270x952
        // (checked 2026-09-23). Same 1564 tokens either way; this port follows the code.
        assert_eq!(api_size(2000, 1500, 1568, 1568), (1270, 952));
        assert_eq!(api_size(3840, 2160, 1568, 1568), (1456, 819));
        assert_eq!(api_size(1920, 1080, 2576, 4784), (1920, 1080));
        assert_eq!(api_size(3840, 2160, 2576, 4784), (2576, 1449));
        assert_eq!(tokens(1456, 819), 1560);
        assert_eq!(tokens(1269, 952), 1564);
        assert_eq!(tokens(2576, 1449), 4784);
        assert_eq!(tokens(1920, 1080), 2691);
        assert_eq!(tokens(1000, 1000), 1296);
    }

    #[test]
    fn square_tiles_respect_the_token_cap() {
        // 2000x2000 would be 72x72 = 5184 tokens > 4784: the API would shrink it.
        let f = page(2000, 9000);
        let p = plan(&f, 1.0, 2000, Tier::High, false, Filter::Box).unwrap();
        assert!(p.tiles.iter().all(|t| tokens(t.w, t.h) <= 4784), "{:?}", p.tiles.iter().map(|t| (t.w, t.h)).collect::<Vec<_>>());
        let p = plan(&f, 1.0, 2000, Tier::Standard, false, Filter::Box).unwrap();
        assert!(p.tiles.iter().all(|t| tokens(t.w, t.h) <= 1568 && t.w.div_ceil(28) * 28 <= 1568));
    }

    #[test]
    fn trim_removes_only_uniform_margins() {
        let mut f = Frame::black(200, 100);
        for y in 40..50 {
            for x in 60..90 {
                let i = ((y * 200 + x) * 3) as usize;
                f.rgb[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let (t, (x, y, w, h)) = trim(&f);
        assert_eq!((x, y, w, h), (54, 34, 42, 22));
        assert_eq!((t.w, t.h), (42, 22));
        let (t, _) = trim(&Frame::black(50, 50));
        assert_eq!((t.w, t.h), (50, 50), "an all-plain image is left alone");
    }
}
