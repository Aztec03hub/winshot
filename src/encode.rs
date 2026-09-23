//! PNG / WebP-lossless encoding, the lossless palette path, and resampling.

use crate::capture::Frame;
use anyhow::Result;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Debug, clap::ValueEnum)]
pub enum Format {
    Png,
    Webp,
}

#[derive(Clone, Copy, PartialEq, Debug, clap::ValueEnum)]
pub enum Filter {
    Lanczos3,
    Mitchell,
    Box,
    Bilinear,
}

pub struct Encoded {
    pub bytes: Vec<u8>,
    /// Some(n) when the lossless palette path was taken with n colours.
    pub palette: Option<usize>,
}

/// Exact palette if the frame has <= 256 colours, else None.
/// Only colour *changes* hit the hash map and it exits at colour 257, so the
/// scan costs ~1 ms on real captures (DESIGN §4), cheap enough to always try.
pub fn palette(f: &Frame) -> Option<(Vec<[u8; 3]>, Vec<u8>)> {
    let mut map: HashMap<[u8; 3], u8> = HashMap::with_capacity(512);
    let mut idx = Vec::with_capacity((f.w * f.h) as usize);
    let mut prev: Option<([u8; 3], u8)> = None;
    for p in f.rgb.chunks_exact(3) {
        let c = [p[0], p[1], p[2]];
        let i = match prev {
            Some((pc, pi)) if pc == c => pi,
            _ => {
                let i = match map.get(&c) {
                    Some(&i) => i,
                    None if map.len() == 256 => return None,
                    None => {
                        let i = map.len() as u8;
                        map.insert(c, i);
                        i
                    }
                };
                prev = Some((c, i));
                i
            }
        };
        idx.push(i);
    }
    let mut pal = vec![[0u8; 3]; map.len()];
    for (c, i) in map {
        pal[i as usize] = c;
    }
    Some((pal, idx))
}

pub fn encode(f: &Frame, fmt: Format) -> Result<Encoded> {
    match fmt {
        Format::Webp => {
            let mut bytes = Vec::new();
            image_webp::WebPEncoder::new(&mut bytes).encode(&f.rgb, f.w, f.h, image_webp::ColorType::Rgb8)?;
            Ok(Encoded { bytes, palette: None })
        }
        Format::Png => {
            let pal = palette(f);
            let mut bytes = Vec::with_capacity(f.rgb.len() / 4);
            {
                let mut e = png::Encoder::new(&mut bytes, f.w, f.h);
                e.set_depth(png::BitDepth::Eight);
                // Balanced: ~2x smaller than Fast for ~50 ms at 9 Mpx; High buys <1% more (DESIGN §4).
                e.set_compression(png::Compression::Balanced);
                match &pal {
                    Some((p, _)) => {
                        e.set_color(png::ColorType::Indexed);
                        e.set_palette(p.concat());
                    }
                    None => e.set_color(png::ColorType::Rgb),
                }
                let mut w = e.write_header()?;
                w.write_image_data(pal.as_ref().map_or(&f.rgb, |(_, idx)| idx))?;
                w.finish()?;
            }
            Ok(Encoded { bytes, palette: pal.map(|(p, _)| p.len()) })
        }
    }
}

/// Largest side accepted for a --region or a --from image: a typo like 40000 would
/// otherwise allocate gigabytes. Twice the widest real desktop is plenty.
pub const MAX_SIDE: u32 = 32768;

pub fn decode_png(bytes: &[u8]) -> Result<Frame> {
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info()?;
    let (iw, ih) = (r.info().width, r.info().height);
    if iw > MAX_SIDE || ih > MAX_SIDE {
        anyhow::bail!("{iw}x{ih} image is larger than {MAX_SIDE} px on a side");
    }
    let mut buf = vec![0; r.output_buffer_size().unwrap_or(0)];
    let info = r.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let rgb = match info.color_type {
        png::ColorType::Rgb => buf,
        png::ColorType::Rgba => buf.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0]]).collect(),
        png::ColorType::Indexed => unreachable!("EXPAND removes indexed"),
    };
    Ok(Frame { w, h, rgb })
}

/// Resample to w x h. `Box` is exact area-averaging for integer ratios like 1/2,
/// and is picked automatically for those when the caller asked for Lanczos3.
pub fn resize(f: &Frame, w: u32, h: u32, filter: Filter) -> Result<Frame> {
    use fast_image_resize::{images::Image, FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    if (w, h) == (f.w, f.h) {
        return Ok(Frame { w, h, rgb: f.rgb.clone() });
    }
    let integer = f.w.is_multiple_of(w) && f.h.is_multiple_of(h) && f.w / w == f.h / h;
    let ft = match filter {
        Filter::Lanczos3 if integer => FilterType::Box,
        Filter::Lanczos3 => FilterType::Lanczos3,
        Filter::Mitchell => FilterType::Mitchell,
        Filter::Box => FilterType::Box,
        Filter::Bilinear => FilterType::Bilinear,
    };
    let src = Image::from_vec_u8(f.w, f.h, f.rgb.clone(), PixelType::U8x3)?;
    let mut dst = Image::new(w, h, PixelType::U8x3);
    Resizer::new().resize(&src, &mut dst, &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(ft)))?;
    Ok(Frame { w, h, rgb: dst.into_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(colours: u32) -> Frame {
        // 64x64, `colours` distinct colours in runs of varying length.
        let rgb = (0..64 * 64u32).flat_map(|i| {
            let c = (i / 3) % colours;
            [(c & 0xff) as u8, (c >> 8) as u8, 7]
        });
        Frame { w: 64, h: 64, rgb: rgb.collect() }
    }

    #[test]
    fn palette_png_round_trips_bit_identical() {
        for n in [1, 2, 200, 256] {
            let f = frame(n);
            let e = encode(&f, Format::Png).unwrap();
            assert_eq!(e.palette, Some(n as usize), "{n} colours must take the palette path");
            assert!(decode_png(&e.bytes).unwrap().rgb == f.rgb, "{n} colours: indexed PNG must decode bit-identical");
        }
    }

    #[test]
    fn colour_257_falls_back_to_truecolor() {
        let f = frame(257);
        let e = encode(&f, Format::Png).unwrap();
        assert_eq!(e.palette, None);
        assert!(decode_png(&e.bytes).unwrap().rgb == f.rgb);
    }

    #[test]
    fn half_scale_box_averages() {
        let f = Frame { w: 2, h: 2, rgb: vec![0, 0, 0, 200, 200, 200, 0, 0, 0, 200, 200, 200] };
        assert_eq!(resize(&f, 1, 1, Filter::Lanczos3).unwrap().rgb, vec![100, 100, 100]);
    }
}
