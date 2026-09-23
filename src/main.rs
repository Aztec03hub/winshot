mod calibrate;
mod capture;
mod encode;
mod fx;
mod tile;

use anyhow::{bail, Context, Result};
use capture::{Capturer, Display, Frame, Rect};
use clap::{Args, Parser, Subcommand};
use encode::{Filter, Format};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE};

#[derive(Parser)]
#[command(version, about = "Windows screenshots for LLMs, driven from WSL: sized so Claude can read every word (--raw for a lossless original)", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    shot: Shot,
}

#[derive(Subcommand)]
enum Cmd {
    /// List displays or windows
    List {
        #[command(subcommand)]
        what: ListWhat,
    },
    /// Preview the capture flash and shutter sound, without capturing anything
    Fx {
        /// display to flash: index, name (DISPLAY1) or "primary"; default every display
        #[arg(long)]
        display: Option<String>,
        /// skip the flash
        #[arg(long)]
        no_flash: bool,
        /// skip the shutter sound
        #[arg(long)]
        no_sound: bool,
        /// also write the synthesized shutter sound to this .wav file
        #[arg(long)]
        wav: Option<PathBuf>,
        /// flash fade length in ms (a capture uses 120; longer is for photographing it)
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u32).range(10..=10000))]
        duration: u32,
    },
    /// Find a display's smallest text-safe downscale by OCR agreement, and cache it for --scale auto
    Calibrate {
        /// display index, name (DISPLAY1) or "primary"; default primary
        #[arg(long)]
        display: Option<String>,
        /// calibrate on a saved PNG instead (prints the result, caches nothing)
        #[arg(long, conflicts_with = "display")]
        from: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ListWhat {
    Displays,
    Windows {
        #[arg(long)]
        filter: Option<String>,
    },
}

#[derive(Args)]
#[group(id = "target", multiple = false)]
struct Target {
    /// one monitor: index, name (DISPLAY1) or "primary"
    #[arg(long)]
    display: Option<String>,
    /// window whose title contains STR (case-insensitive), or an hwnd like 0x4114c
    #[arg(long)]
    window: Option<String>,
    /// top window of this process id
    #[arg(long)]
    pid: Option<u32>,
    /// top window of this executable (capcut or capcut.exe)
    #[arg(long)]
    exe: Option<String>,
    /// x,y,w,h in physical virtual-desktop pixels
    #[arg(long, value_parser = parse_region, allow_hyphen_values = true)]
    region: Option<Rect>,
    /// every display, one file each
    #[arg(long)]
    all: bool,
    /// process a saved PNG instead of capturing (e.g. a full-page browser capture)
    #[arg(long)]
    from: Option<PathBuf>,
}

#[derive(Args)]
struct Shot {
    #[command(flatten)]
    target: Target,
    /// output path (default ./winshot-<timestamp>.<ext>)
    #[arg(short, long)]
    out: Option<PathBuf>,
    /// png or webp (lossless); default from -o's extension, else png
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// lossless, full-resolution capture for people: one PNG exactly as on screen.
    /// Without it (the default) output is sized for an LLM to read: the smallest text is
    /// measured and kept legible, and long images become tiles nothing downstream resizes
    #[arg(long)]
    raw: bool,
    /// the default mode; accepted so older commands keep working
    #[arg(long, hide = true, conflicts_with = "raw")]
    for_claude: bool,
    /// long-edge cap in px (Claude Code's own cap is 2000)
    #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u32).range(256..=tile::MAX_EDGE as i64))]
    long_edge: u32,
    /// Claude resolution tier the output is sized for (and the token figures in
    /// --verbose): high = Claude 4.7 and later
    #[arg(long, value_enum, default_value = "high")]
    tier: tile::Tier,
    /// keep images that already fit at full size instead of shrinking them to their
    /// text-safe scale (more tokens, more non-text detail)
    #[arg(long, conflicts_with = "raw")]
    max_detail: bool,
    /// skip measuring and use this text-safe scale
    #[arg(long, value_parser = parse_factor, conflicts_with = "raw")]
    text_scale: Option<f64>,
    /// --raw only: resampling off (default), auto (calibrated, else 1/DPI), or a factor like 0.5
    #[arg(long, value_parser = parse_scale, requires = "raw")]
    scale: Option<Scale>,
    /// --raw only: downscale to fit WxH, aspect preserved, never upscales
    #[arg(long, value_parser = parse_wh, requires = "raw")]
    fit: Option<(u32, u32)>,
    /// resampling filter (lanczos3 uses exact area-averaging for 1/2, 1/4, ...)
    #[arg(long, value_enum, default_value = "lanczos3")]
    filter: Filter,
    /// white flash over what was captured, after the capture
    #[arg(long)]
    flash: bool,
    /// camera-shutter click, after the capture
    #[arg(long)]
    shutter: bool,
    /// wait this many milliseconds before capturing
    #[arg(long, default_value_t = 0)]
    delay: u64,
    /// include the mouse cursor
    #[arg(long)]
    cursor: bool,
    /// write the image to stdout instead of a file
    #[arg(long, conflicts_with = "out")]
    stdout: bool,
    /// report resolution, DPI, colour path, sizes and timings on stderr
    #[arg(short, long)]
    verbose: bool,
    /// benchmark every encoder setting on this capture and exit
    #[arg(long, hide = true, requires = "raw")]
    bench_encode: bool,
    /// print every OCR text line (height, shape, position, words) and exit (analysis)
    #[arg(long, hide = true)]
    dump_text: bool,
}

#[derive(Clone, Copy)]
enum Scale {
    Off,
    Auto,
    Factor(f64),
}

fn parse_scale(s: &str) -> Result<Scale, String> {
    match s {
        "off" => Ok(Scale::Off),
        "auto" => Ok(Scale::Auto),
        _ => parse_factor(s).map(Scale::Factor).map_err(|_| "expected off, auto, or a factor in (0, 1]".into()),
    }
}

fn parse_factor(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(f) if f > 0.0 && f <= 1.0 => Ok(f),
        _ => Err("expected a factor in (0, 1]".into()),
    }
}

fn parse_nums(s: &str, sep: char, n: usize) -> Result<Vec<i64>, String> {
    let v: Vec<i64> = s.split(sep).map(|p| p.trim().parse::<i64>()).collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    if v.len() != n {
        return Err(format!("expected {n} numbers separated by '{sep}'"));
    }
    Ok(v)
}

fn parse_region(s: &str) -> Result<Rect, String> {
    let v = parse_nums(s, ',', 4)?;
    if v[2] <= 0 || v[3] <= 0 {
        return Err("width and height must be positive".into());
    }
    if v[2] > encode::MAX_SIDE as i64 || v[3] > encode::MAX_SIDE as i64 || (v[2] * v[3]) as u64 > encode::MAX_PIXELS {
        return Err(format!("width and height must be at most {} px, and the area at most {} pixels", encode::MAX_SIDE, encode::MAX_PIXELS));
    }
    Ok(Rect { x: v[0] as i32, y: v[1] as i32, w: v[2] as u32, h: v[3] as u32 })
}

fn parse_wh(s: &str) -> Result<(u32, u32), String> {
    let v = parse_nums(&s.to_lowercase(), 'x', 2)?;
    if v[0] <= 0 || v[1] <= 0 {
        return Err("both dimensions must be positive".into());
    }
    Ok((v[0] as u32, v[1] as u32))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("winshot: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    capture::assert_dpi_aware()?;
    unsafe { windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED)? };
    match cli.cmd {
        Some(Cmd::List { what: ListWhat::Displays }) => list_displays(),
        Some(Cmd::List { what: ListWhat::Windows { filter } }) => list_windows(filter),
        Some(Cmd::Calibrate { display, from }) => run_calibrate(display, from),
        Some(Cmd::Fx { display, no_flash, no_sound, wav, duration }) => run_fx(display, no_flash, no_sound, wav, duration),
        None => shoot(cli.shot),
    }
}

fn list_displays() -> Result<()> {
    let ds = capture::displays()?;
    println!("{:<3} {:<10} {:>11} {:>11} {:>6} {:>12} hdr", "#", "name", "physical", "logical", "scale", "position");
    for d in &ds {
        let (lw, lh) = d.logical();
        println!(
            "{:<3} {:<10} {:>11} {:>11} {:>5.0}% {:>12} {:<4} {}",
            d.index,
            d.name,
            format!("{}x{}", d.rect.w, d.rect.h),
            format!("{lw}x{lh}"),
            d.scale_factor() * 100.0,
            format!("{},{}", d.rect.x, d.rect.y),
            if d.hdr { "yes" } else { "no" },
            if d.primary { "primary" } else { "" }
        );
    }
    let v = Rect::union(ds.iter().map(|d| d.rect));
    println!("virtual desktop {}x{} physical at {},{}", v.w, v.h, v.x, v.y);
    Ok(())
}

fn list_windows(filter: Option<String>) -> Result<()> {
    let ds = capture::displays()?;
    let f = filter.map(|s| s.to_lowercase());
    println!("{:<10} {:>7} {:<24} {:<3} {:<5} title", "hwnd", "pid", "exe", "mon", "state");
    for w in capture::windows()? {
        let hay = format!("{} {}", w.title, w.exe).to_lowercase();
        if f.as_ref().is_some_and(|f| !hay.contains(f.as_str())) {
            continue;
        }
        let mon = ds.iter().find(|d| d.hmon == w.hmon).map_or(0, |d| d.index);
        let state = if w.minimized { "min" } else if w.maximized { "max" } else { "" };
        println!("{:<10} {:>7} {:<24} {:<3} {:<5} {}", format!("{:#x}", w.hwnd.0 as usize), w.pid, w.exe, mon, state, w.title);
    }
    Ok(())
}

fn find_display<'a>(ds: &'a [Display], sel: &str) -> Result<&'a Display> {
    let found = match sel.to_lowercase().as_str() {
        "primary" => ds.iter().find(|d| d.primary),
        s => match s.parse::<usize>() {
            Ok(i) => ds.iter().find(|d| d.index == i),
            Err(_) => ds.iter().find(|d| d.name.eq_ignore_ascii_case(s)),
        },
    };
    found.with_context(|| format!("no display '{sel}' (see `winshot list displays`)"))
}

fn run_fx(display: Option<String>, no_flash: bool, no_sound: bool, wav: Option<PathBuf>, duration: u32) -> Result<()> {
    if let Some(p) = &wav {
        let bytes = fx::shutter_wav();
        std::fs::write(p, &bytes).with_context(|| format!("writing {}", p.display()))?;
        println!("wav: {} ({} bytes, 44100 Hz mono 16-bit)", p.display(), bytes.len());
    }
    let ds = capture::displays()?;
    let rects: Vec<Rect> = match &display {
        Some(sel) => vec![find_display(&ds, sel)?.rect],
        None => ds.iter().map(|d| d.rect).collect(),
    };
    let t = Instant::now();
    let flash = (!no_flash).then(|| fx::flash(rects.clone(), duration));
    let sound = (!no_sound).then(fx::shutter);
    if let Some(h) = flash {
        let _ = h.join();
        println!("flash: {} display(s), {} ms fade, done after {:.0} ms", rects.len(), duration, t.elapsed().as_secs_f64() * 1e3);
    }
    if let Some(h) = sound {
        match h.join() {
            Ok(true) => println!("shutter: played (Windows PlaySound reported success)"),
            _ => bail!("shutter: Windows PlaySound reported failure (no audio device, or the sound was blocked)"),
        }
    }
    Ok(())
}

fn run_calibrate(display: Option<String>, from: Option<PathBuf>) -> Result<()> {
    let (frame, name) = match from {
        Some(p) => (encode::decode_png(&std::fs::read(&p).with_context(|| p.display().to_string())?)?, None),
        None => {
            let ds = capture::displays()?;
            let d = find_display(&ds, display.as_deref().unwrap_or("primary"))?;
            eprintln!("calibrating {} ({}x{}, {:.0}%) - keep text-dense content (a terminal, an editor) on it", d.name, d.rect.w, d.rect.h, d.scale_factor() * 100.0);
            (capture_display(&mut Capturer::new(false)?, d)?, Some((d.name.clone(), 1.0 / d.scale_factor())))
        }
    };
    let c = calibrate::calibrate(&frame)?;
    eprintln!(
        "smallest text: {:.0} px tall, {} of {} lines, {} words; scoring only those",
        c.small.h,
        c.small.lines,
        c.small.all_lines,
        c.small.words.len()
    );
    for (s, a) in &c.steps {
        eprintln!("  scale {s:.3}: smallest text recall {:.1}%", a * 100.0);
    }
    println!(
        "safe scale {:.3}: smallest where OCR still reads >= {:.0}% of the smallest text it reads at x0.99 ({:.1}%)",
        c.scale,
        calibrate::RETAIN * 100.0,
        c.floor * 100.0
    );
    if let Some((name, dpi_scale)) = name {
        println!("DPI-derived scale for comparison: {dpi_scale:.3}");
        println!("cached {name} = {:.3} in {}", c.scale, calibrate::store(&name, c.scale)?.display());
    }
    Ok(())
}

/// Monitor grab, with the check that would have caught the v0 bug: the frame
/// must be exactly the monitor's physical size.
fn capture_display(cap: &mut Capturer, d: &Display) -> Result<Frame> {
    let f = cap.monitor(d.hmon)?;
    if (f.w, f.h) != (d.rect.w, d.rect.h) {
        bail!("{}: captured {}x{} but the display is {}x{} physical; refusing to save a rescaled frame", d.name, f.w, f.h, d.rect.w, d.rect.h);
    }
    Ok(f)
}

/// Composite every display intersecting `area` at its physical position.
fn capture_area(cap: &mut Capturer, ds: &[Display], area: Rect) -> Result<(Frame, Vec<usize>)> {
    let mut canvas = Frame::black(area.w, area.h);
    let mut used = Vec::new();
    for d in ds.iter().filter(|d| d.rect.intersects(&area)) {
        canvas.blit(&capture_display(cap, d)?, d.rect.x - area.x, d.rect.y - area.y);
        used.push(d.index);
    }
    if used.is_empty() {
        bail!("region {},{} {}x{} is not on any display", area.x, area.y, area.w, area.h);
    }
    Ok((canvas, used))
}

struct Job {
    frame: Frame,
    displays: Vec<usize>, // indices involved, for --scale auto
    rect: Rect,           // on-screen area, for --flash
    label: String,
    suffix: Option<String>, // --all file-name suffix
}

fn shoot(o: Shot) -> Result<()> {
    let t0 = Instant::now();
    let ds = capture::displays()?;
    let t = &o.target;
    let mut jobs = Vec::new();
    let mut t_dev = Duration::ZERO;
    let mut borderless = None;
    if let Some(p) = &t.from {
        // A saved image: no capture. Its text scale is measured; --scale auto and the
        // no-text fallback assume it came from the primary display.
        let frame = encode::decode_png(&std::fs::read(p).with_context(|| p.display().to_string())?).with_context(|| format!("{} (PNG only)", p.display()))?;
        let primary = ds.iter().find(|d| d.primary).map(|d| d.index).into_iter().collect();
        jobs.push(Job { frame, displays: primary, rect: Rect { x: 0, y: 0, w: 0, h: 0 }, label: p.display().to_string(), suffix: None });
    } else {
        if o.delay > 0 {
            std::thread::sleep(Duration::from_millis(o.delay));
        }
        let td = Instant::now();
        let mut cap = Capturer::new(o.cursor)?;
        t_dev = td.elapsed();
        let chosen = t.display.is_some() || t.region.is_some() || t.window.is_some() || t.pid.is_some() || t.exe.is_some();
        // For Claude, the whole desktop splits at monitor edges first: each monitor is
        // already a screen-shaped image, so no cut lands inside anything.
        if !o.raw && !chosen && o.stdout {
            // One image to stdout: the primary display, not every monitor.
            let d = find_display(&ds, "primary")?;
            jobs.push(Job { frame: capture_display(&mut cap, d)?, displays: vec![d.index], rect: d.rect, label: d.name.clone(), suffix: None });
        } else if t.all || (!o.raw && !chosen) {
            if o.stdout && t.all {
                bail!("--all writes one file per display; it cannot go to --stdout");
            }
            for d in &ds {
                let frame = capture_display(&mut cap, d)?;
                jobs.push(Job { frame, displays: vec![d.index], rect: d.rect, label: d.name.clone(), suffix: Some(d.name.clone()) });
            }
        } else if let Some(sel) = &t.display {
            let d = find_display(&ds, sel)?;
            jobs.push(Job { frame: capture_display(&mut cap, d)?, displays: vec![d.index], rect: d.rect, label: d.name.clone(), suffix: None });
        } else if let Some(r) = t.region {
            let (frame, displays) = capture_area(&mut cap, &ds, r)?;
            jobs.push(Job { frame, displays, rect: r, label: "region".into(), suffix: None });
        } else if t.window.is_some() || t.pid.is_some() || t.exe.is_some() {
            jobs.push(capture_window(&mut cap, &ds, t, o.verbose)?);
        } else {
            let v = Rect::union(ds.iter().map(|d| d.rect));
            let (frame, displays) = capture_area(&mut cap, &ds, v)?;
            jobs.push(Job { frame, displays, rect: v, label: "virtual desktop".into(), suffix: None });
        }
        borderless = cap.borderless;
    }
    let t_cap = t0.elapsed();

    // Every frame is in memory now, so feedback cannot end up in a screenshot.
    let (mut flash, mut shutter) = (None, None);
    if o.flash && t.from.is_none() {
        flash = Some(fx::flash(jobs.iter().map(|j| j.rect).collect(), 120));
    }
    if o.shutter {
        shutter = Some(fx::shutter());
    }

    if o.verbose {
        for d in ds.iter().filter(|d| t.from.is_none() && jobs.iter().any(|j| j.displays.contains(&d.index))) {
            let (lw, lh) = d.logical();
            eprintln!("{}: {}x{} physical ({lw}x{lh} logical, {:.0}%){}", d.name, d.rect.w, d.rect.h, d.scale_factor() * 100.0, if d.hdr { ", HDR (Windows converts to SDR)" } else { "" });
        }
        if borderless == Some(false) {
            eprintln!("note: this Windows build refused IsBorderRequired=false; a yellow capture border may be visible");
        }
        eprintln!("capture: {:.0} ms (of which D3D device {:.0} ms)", t_cap.as_secs_f64() * 1e3, t_dev.as_secs_f64() * 1e3);
    }

    let format = o.format.unwrap_or(match o.out.as_ref().and_then(|p| p.extension()).and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("webp") => Format::Webp,
        _ => Format::Png,
    });
    let base = o.out.clone().unwrap_or_else(|| format!("winshot-{}.{}", timestamp(), if format == Format::Webp { "webp" } else { "png" }).into());
    let calib = calibrate::load();

    if o.dump_text {
        for job in &jobs {
            println!("# {} ({}x{}): box_px real_px shape x,y wxh words", job.label, job.frame.w, job.frame.h);
            calibrate::dump(&job.frame)?;
        }
        return Ok(());
    }
    let jobs_n = jobs.len();
    for job in jobs {
        if job.frame.rgb.iter().all(|&b| b == 0) {
            eprintln!("winshot: warning: {} captured as an entirely black frame (DRM-protected content, or a capture failure); saving it anyway", job.label);
        }
        let path = |extra: Option<String>| {
            let parts: Vec<String> = job.suffix.iter().cloned().chain(extra).collect();
            if parts.is_empty() {
                base.clone()
            } else {
                with_suffix(&base, &parts.join("-"))
            }
        };
        if !o.raw {
            let tiles = for_claude(&o, &ds, &calib, &job)?;
            let n = tiles.len();
            if o.stdout && jobs_n > 1 {
                bail!("this capture came out as {jobs_n} displays, which cannot all go to --stdout; pick one with --display, write files with -o, or use --raw for one lossless image");
            }
            if o.stdout && n > 1 {
                bail!("this capture came out as {n} images, which cannot all go to --stdout; write files with -o, or use --raw for one lossless image");
            }
            for (i, tile) in tiles.iter().enumerate() {
                emit(&o, format, tile, &path((n > 1).then(|| format!("{}of{n}", i + 1))), &job.label)?;
            }
            continue;
        }
        let mut f = job.frame;
        let factor = scale_for(&o, &ds, &job.displays, &calib, &f);
        let mut unscaled = None;
        if factor < 1.0 {
            let t = Instant::now();
            let small = tile::scaled(&f, factor, o.filter)?;
            if o.verbose {
                eprintln!("resize: x{factor:.3} -> {}x{} ({:?}), {:.0} ms", small.w, small.h, o.filter, t.elapsed().as_secs_f64() * 1e3);
            }
            unscaled = Some(std::mem::replace(&mut f, small));
        }
        if o.bench_encode {
            return bench_encode(&f);
        }
        if f.w.max(f.h) > tile::MAX_EDGE {
            eprintln!("winshot: warning: {}x{} exceeds Claude's {} px limit and would be rejected there; drop --raw to get tiles Claude can read", f.w, f.h, tile::MAX_EDGE);
        }
        let bytes = emit(&o, format, &f, &path(None), &job.label)?;
        // Resampling invents in-between colours that compress worse, so a mild downscale
        // can cost resolution AND bytes (x0.75 measured 9-40% bigger than raw, DESIGN §5).
        if let Some(u) = unscaled {
            let full = encode::encode(&u, format)?.bytes.len();
            if bytes >= full {
                eprintln!("winshot: warning: the x{factor:.3} downscale is {bytes} bytes, not smaller than the full-resolution {full}; it lost detail for nothing (drop --scale/--fit)");
            }
        }
    }
    if let Some(h) = flash {
        let _ = h.join();
    }
    if let Some(h) = shutter {
        if !h.join().unwrap_or(false) {
            eprintln!("winshot: warning: the shutter sound did not play (Windows PlaySound reported failure)");
        }
    }
    if o.verbose {
        eprintln!("total: {:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
    }
    Ok(())
}

/// Encode and write (or stream) one image; returns its size in bytes.
fn emit(o: &Shot, format: Format, f: &Frame, path: &Path, label: &str) -> Result<usize> {
    let t = Instant::now();
    let enc = encode::encode(f, format)?;
    let t_enc = t.elapsed();
    let t = Instant::now();
    if o.stdout {
        std::io::stdout().lock().write_all(&enc.bytes)?;
    } else {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // One sequential write: cheap even onto a \\wsl.localhost path (DESIGN §1).
        std::fs::write(path, &enc.bytes).with_context(|| format!("writing {}", path.display()))?;
        println!("{} ({}x{})", path.display(), f.w, f.h);
    }
    if o.verbose {
        let kind = match enc.palette {
            Some(n) => format!("palette PNG, {n} colours (lossless)"),
            None if format == Format::Webp => "WebP lossless".into(),
            None => "truecolor PNG (>256 colours)".into(),
        };
        let (cw, ch) = tile::fit_size(f.w, f.h, o.long_edge, o.tier);
        let seen = if (cw, ch) == (f.w, f.h) { "unresized".to_string() } else { format!("resized to {cw}x{ch}") };
        eprintln!(
            "{label}: {}x{}, {kind}, {} bytes; encode {:.0} ms, write {:.0} ms; Claude ({:?} tier, {} px cap): {seen}, {} tokens",
            f.w,
            f.h,
            enc.bytes.len(),
            t_enc.as_secs_f64() * 1e3,
            t.elapsed().as_secs_f64() * 1e3,
            o.tier,
            o.long_edge,
            tile::billed(cw, ch)
        );
    }
    Ok(enc.bytes.len())
}

/// Claude's vision limits without losing text: measure the smallest text on this
/// image, then fit the long edge, tiling instead of shrinking past legibility.
fn for_claude(o: &Shot, ds: &[Display], calib: &std::collections::BTreeMap<String, f64>, job: &Job) -> Result<Vec<Frame>> {
    let t = Instant::now();
    // Uniform margins carry no content but cost a token per 28 px patch: crop them first.
    let (trimmed, (tx, ty, tw, th)) = tile::trim(&job.frame);
    if o.verbose && (tw, th) != (job.frame.w, job.frame.h) {
        eprintln!("for-claude: {}: trimmed uniform margins {}x{} -> {tw}x{th} at {tx},{ty}", job.label, job.frame.w, job.frame.h);
    }
    let f = &trimmed;
    // As-is baseline: what sending the untouched capture would cost and look like.
    let (bw, bh) = tile::fit_size(job.frame.w, job.frame.h, o.long_edge, o.tier);
    let fit = tile::fit_size(f.w, f.h, o.long_edge, o.tier).0 as f64 / f.w as f64;
    let (text, how) = if let Some(s) = o.text_scale {
        (s, "--text-scale".to_string())
    } else if fit >= 1.0 && o.max_detail {
        (1.0, "already fits, nothing to shrink".to_string())
    } else {
        match calibrate::text_scale(f)? {
            Some(v) => {
                let shape = v.shape.unwrap_or(calibrate::Shape::Text);
                let how = format!(
                    "smallest text {:.0} px tall (real height, estimated from its OCR box; {:?}, {} of {} lines, e.g. \"{}\"), kept at >= {} px: Claude's measured threshold for {:?}",
                    v.small.h,
                    shape,
                    v.small.lines,
                    v.small.all_lines,
                    v.small.words.iter().take(8).cloned().collect::<Vec<_>>().join(" "),
                    shape.safe_px(),
                    shape
                );
                (v.scale, how)
            }
            None => {
                let s = auto_scale(ds, &job.displays, calib);
                eprintln!("winshot: warning: {}: too little readable text to measure; using the calibrated or 1/DPI scale x{s:.3} instead", job.label);
                (s, "too little readable text to measure; calibrated or 1/DPI fallback".into())
            }
        }
    };
    let plan = tile::plan(f, text, o.long_edge, o.tier, !o.max_detail, o.filter)?;
    let n = plan.tiles.len();
    let tokens: u32 = plan.tiles.iter().map(|t| tile::billed(t.w, t.h)).sum();
    if o.verbose {
        eprintln!("for-claude: {}: text-safe x{text:.3} ({how}), {:.0} ms", job.label, t.elapsed().as_secs_f64() * 1e3);
        let blank = plan.seams.iter().filter(|&&b| b).count();
        eprintln!(
            "for-claude: sent as-is it would arrive as {bw}x{bh} = {} tokens (x{:.3}); output {n} image(s) at x{:.3}{} = {tokens} tokens",
            tile::billed(bw, bh),
            bw as f64 / job.frame.w as f64,
            plan.scale,
            if n > 1 { format!(", {blank} seam(s) in blank rows, {} overlapped", plan.seams.len() - blank) } else { String::new() }
        );
    }
    if plan.short_edge_limited {
        eprintln!("winshot: warning: even the short edge exceeds {} px at the text-safe scale; shrunk to x{:.3}, text may be below legible size", plan.edge, plan.scale);
    }
    if n > 8 {
        eprintln!("winshot: note: {n} images, ~{tokens} tokens for Claude to read; consider capturing less");
    }
    Ok(plan.tiles)
}

/// --scale auto per display: calibrated, else 1/DPI; for several displays the least
/// aggressive, so no display's text drops below its safe size.
fn auto_scale(ds: &[Display], involved: &[usize], calib: &std::collections::BTreeMap<String, f64>) -> f64 {
    let s = ds.iter().filter(|d| involved.contains(&d.index)).map(|d| calib.get(&d.name).copied().unwrap_or(1.0 / d.scale_factor())).fold(0.0, f64::max);
    if s > 0.0 {
        s.min(1.0)
    } else {
        1.0
    }
}

fn capture_window(cap: &mut Capturer, ds: &[Display], t: &Target, verbose: bool) -> Result<Job> {
    let all = capture::windows()?;
    let want = |w: &&capture::Window| {
        if let Some(s) = &t.window {
            match s.strip_prefix("0x").and_then(|h| usize::from_str_radix(h, 16).ok()) {
                Some(h) => w.hwnd.0 as usize == h,
                None => w.title.to_lowercase().contains(&s.to_lowercase()),
            }
        } else if let Some(p) = t.pid {
            w.pid == p
        } else {
            let e = t.exe.as_deref().unwrap_or_default().to_lowercase();
            let exe = w.exe.to_lowercase();
            exe == e || exe == format!("{e}.exe")
        }
    };
    let hits: Vec<_> = all.iter().filter(want).collect();
    if hits.is_empty() {
        bail!("no matching window (see `winshot list windows`)");
    }
    // Apps like CapCut own visible-but-blank helper windows above the real one:
    // take the topmost match whose frame is not entirely black.
    let (mut first_black, mut first_err) = (None, None);
    for (i, w) in hits.iter().enumerate() {
        // A helper window that fails to capture is skipped like a blank one.
        let job = match capture_one_window(cap, ds, w, verbose) {
            Ok(job) => job,
            Err(e) => {
                if verbose {
                    eprintln!("winshot: {:?} ({:#x}) failed to capture, trying the next match: {e:#}", w.title, w.hwnd.0 as usize);
                }
                first_err.get_or_insert(e);
                continue;
            }
        };
        if job.frame.rgb.iter().any(|&b| b != 0) {
            if hits.len() > 1 {
                eprintln!("winshot: {} windows match; captured {:?} ({}, {:#x}){}", hits.len(), w.title, w.exe, w.hwnd.0 as usize, if i > 0 { format!(", skipping {i} blank one(s) above it") } else { String::new() });
            }
            return Ok(job);
        }
        first_black.get_or_insert(job);
    }
    match (first_black, first_err) {
        (Some(job), _) => Ok(job),
        (None, Some(e)) => Err(e.context(format!("all {} matching windows failed to capture", hits.len()))),
        (None, None) => unreachable!("hits is non-empty"),
    }
}

fn capture_one_window(cap: &mut Capturer, ds: &[Display], w: &capture::Window, verbose: bool) -> Result<Job> {
    // WGC cannot capture a minimized window: restore it without stealing focus, then put it back.
    if w.minimized {
        unsafe {
            let _ = ShowWindow(w.hwnd, SW_SHOWNOACTIVATE);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let frame = cap.window(w.hwnd);
    let rect = capture::window_rect(w.hwnd); // while restored: a minimized window's bounds are its title stub
    if w.minimized {
        unsafe {
            let _ = ShowWindow(w.hwnd, SW_SHOWMINNOACTIVE);
        }
    }
    let frame = frame?;
    let rect = rect.unwrap_or(Rect { x: 0, y: 0, w: frame.w, h: frame.h });
    // Golden check: the frame must match the DPI-aware window bounds. A ~50% mismatch is the v0 bug class.
    let off = |a: u32, b: u32| (a as f64 - b as f64).abs() / b.max(1) as f64 > 0.1;
    if off(frame.w, rect.w) || off(frame.h, rect.h) {
        eprintln!("winshot: warning: window frame {}x{} differs from its physical bounds {}x{}", frame.w, frame.h, rect.w, rect.h);
    } else if verbose && (frame.w, frame.h) != (rect.w, rect.h) {
        eprintln!("window frame {}x{} vs bounds {}x{} (WGC includes/excludes the frame edge)", frame.w, frame.h, rect.w, rect.h);
    }
    let displays = ds.iter().filter(|d| d.hmon == w.hmon).map(|d| d.index).collect();
    Ok(Job { frame, displays, rect, label: format!("window {:?}", w.title), suffix: None })
}

/// Combined downscale factor from --scale and --fit, never above 1.
fn scale_for(o: &Shot, ds: &[Display], involved: &[usize], calib: &std::collections::BTreeMap<String, f64>, f: &Frame) -> f64 {
    let s = match o.scale.unwrap_or(Scale::Off) {
        Scale::Off => 1.0,
        Scale::Factor(x) => x,
        Scale::Auto => auto_scale(ds, involved, calib),
    };
    let fit = o.fit.map_or(1.0, |(w, h)| (w as f64 / f.w as f64).min(h as f64 / f.h as f64));
    s.min(fit).clamp(f64::MIN_POSITIVE, 1.0)
}

fn with_suffix(p: &Path, s: &str) -> PathBuf {
    let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("winshot");
    let name = match p.extension().and_then(|x| x.to_str()) {
        Some(e) => format!("{stem}-{s}.{e}"),
        None => format!("{stem}-{s}"),
    };
    p.with_file_name(name)
}

fn timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn bench_encode(f: &Frame) -> Result<()> {
    let t = Instant::now();
    let pal = encode::palette(f);
    eprintln!("{}x{} ({:.1} Mpx); colour scan {:.1} ms -> {}", f.w, f.h, (f.w * f.h) as f64 / 1e6, t.elapsed().as_secs_f64() * 1e3, pal.as_ref().map_or("> 256 colours".into(), |(p, _)| format!("{} colours", p.len())));
    for (name, c) in [("fast", png::Compression::Fast), ("balanced", png::Compression::Balanced), ("high", png::Compression::High)] {
        for indexed in [false, true] {
            if indexed && pal.is_none() {
                continue;
            }
            let t = Instant::now();
            let mut bytes = Vec::new();
            let mut e = png::Encoder::new(&mut bytes, f.w, f.h);
            e.set_compression(c);
            match &pal {
                Some((p, _)) if indexed => {
                    e.set_color(png::ColorType::Indexed);
                    e.set_palette(p.concat());
                }
                _ => e.set_color(png::ColorType::Rgb),
            }
            let mut w = e.write_header()?;
            w.write_image_data(if indexed { &pal.as_ref().unwrap().1 } else { &f.rgb })?;
            w.finish()?;
            eprintln!("png {name:<8} {:<10} {:>9} bytes {:>6.0} ms", if indexed { "indexed" } else { "truecolor" }, bytes.len(), t.elapsed().as_secs_f64() * 1e3);
        }
    }
    let t = Instant::now();
    let webp = encode::encode(f, Format::Webp)?;
    eprintln!("webp lossless           {:>9} bytes {:>6.0} ms", webp.bytes.len(), t.elapsed().as_secs_f64() * 1e3);
    Ok(())
}
