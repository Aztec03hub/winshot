//! `winshot hotkey`: a global hotkey that captures the focused display, saves it under a
//! unique name, and puts it on the clipboard (image + file + path text, all at once).
//!
//! The daemon is one hidden message-only window and a message loop. `RegisterHotKey` is
//! system-wide and needs no admin rights; it installs no keyboard hook, except for the short
//! time a remote-desktop window has focus (see `rdp`). The same window is the
//! single-instance lock and the target of `--stop`.

use crate::capture::{self, Capturer, Display, Frame};
use crate::encode::{self, Format};
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS: PCWSTR = w!("winshot_hotkey");
const RUN_VALUE: &str = "winshot-hotkey";
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
// Standard clipboard formats (winuser.h), spelled out so no extra windows feature is needed.
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;

#[derive(clap::Args)]
pub struct HotkeyArgs {
    /// key combination: ctrl, alt, shift, win plus one key (a letter, digit, symbol such as ` or ., F1-F24, printscreen; the + key itself cannot be bound)
    #[arg(long, default_value = "ctrl+`")]
    key: String,
    /// where screenshots are saved (default: %USERPROFILE%\Pictures\winshot)
    #[arg(long)]
    dir: Option<PathBuf>,
    /// delete winshot screenshots in --dir older than this many days (default: keep everything)
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    prune_days: Option<u32>,
    /// no white flash on capture
    #[arg(long)]
    no_flash: bool,
    /// no shutter sound on capture
    #[arg(long)]
    no_sound: bool,
    /// do not install the keyboard hook that makes the key work while a remote-desktop window has focus
    #[arg(long)]
    no_rdp_hook: bool,
    /// stop the running hotkey
    #[arg(long, conflicts_with_all = ["status", "autostart"])]
    stop: bool,
    /// say whether the hotkey is running
    #[arg(long, conflicts_with = "autostart")]
    status: bool,
    /// print where the log is and its last 25 lines
    #[arg(long, conflicts_with_all = ["stop", "status", "autostart"])]
    log: bool,
    /// start the hotkey at every logon (on) or stop doing so (off); the other options are remembered
    #[arg(long, value_enum)]
    autostart: Option<Toggle>,
    /// run the hotkey in this process instead of detaching (what the detached copy runs)
    #[arg(long, hide = true)]
    foreground: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Toggle {
    On,
    Off,
}

impl HotkeyArgs {
    /// The options a detached or autostarted copy needs to behave like this one.
    fn passthrough(&self) -> Vec<String> {
        let mut a = vec!["hotkey".to_string(), "--key".into(), self.key.clone()];
        if self.dir.is_some() {
            if let Ok(d) = self.dir() {
                a.extend(["--dir".into(), d.display().to_string()]);
            }
        }
        if let Some(n) = self.prune_days {
            a.extend(["--prune-days".into(), n.to_string()]);
        }
        a.extend(self.no_flash.then(|| "--no-flash".to_string()));
        a.extend(self.no_sound.then(|| "--no-sound".to_string()));
        a.extend(self.no_rdp_hook.then(|| "--no-rdp-hook".to_string()));
        a
    }

    /// Always absolute: it is stored in the Run key and resolved again at logon, from another
    /// working directory, and the clipboard must carry full paths.
    fn dir(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(d) => std::path::absolute(d).with_context(|| format!("--dir {}", d.display())),
            None => Ok(PathBuf::from(std::env::var("USERPROFILE").context("USERPROFILE is not set; pass --dir")?).join("Pictures").join("winshot")),
        }
    }
}

/// One argument as CommandLineToArgvW will read it back: quoted, inner quotes escaped, and a
/// trailing run of backslashes doubled so it cannot swallow the closing quote.
fn quote_arg(s: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in s.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

pub fn run(a: HotkeyArgs) -> Result<()> {
    if a.stop {
        let Some(h) = running() else {
            println!("the hotkey is not running");
            return Ok(());
        };
        unsafe { PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0))? };
        // The daemon handles WM_CLOSE between presses (a grab is a fraction of a second) and then
        // finishes saving what it already grabbed, so wait until it is really gone: a restart or an
        // install right after must not race it.
        for _ in 0..100 {
            if running().is_none() {
                std::thread::sleep(Duration::from_millis(200)); // the process still has to exit and release its exe
                println!("stopped");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("asked the hotkey to stop, but it is still running after 5 s");
    }
    if a.status {
        println!("{}", if running().is_some() { "running" } else { "not running" });
        return Ok(());
    }
    if a.log {
        let p = log_path();
        println!("{}", p.display());
        let text = String::from_utf8_lossy(&std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        for l in &lines[lines.len().saturating_sub(25)..] {
            println!("{l}");
        }
        return Ok(());
    }
    // Removing the autostart entry needs no valid key or folder (a layout change must not trap it there).
    if let Some(Toggle::Off) = a.autostart {
        return autostart(&a, Toggle::Off);
    }
    parse_key(&a.key)?; // a bad key is an error here, not silence in a hidden process or at every logon
    a.dir()?;
    if let Some(t) = a.autostart {
        return autostart(&a, t);
    }
    if a.foreground {
        return daemon(&a);
    }
    if running().is_some() {
        println!("the hotkey is already running (winshot hotkey --stop first to change its options)");
        return Ok(());
    }
    // Detach: no console window, outlives this shell. Its failures go to the log.
    // CreateProcess hands a child EVERY inheritable handle, not only the std ones, so a
    // pipe from the WSL shim would stay open for as long as the daemon lives. Close that door.
    unsafe {
        for std in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            if let Ok(h) = GetStdHandle(std) {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
            }
        }
    }
    let log_start = std::fs::metadata(log_path()).map_or(0, |m| m.len()); // only lines written after this are this run's
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(a.passthrough())
        .arg("--foreground")
        // Null stdio: an inherited pipe (the WSL shim's) would never reach EOF while the daemon lives.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0000_0008) // DETACHED_PROCESS: no console of its own, outlives this one
        .spawn()?;
    for _ in 0..40 {
        if running().is_some() {
            // The window exists a moment before the key is registered: let a failing child finish dying.
            std::thread::sleep(Duration::from_millis(150));
            if let Some(st) = child.try_wait()? {
                bail!("the hotkey process exited ({st}): {}", new_log_line(log_start));
            }
            println!("hotkey {} is active; screenshots go to {}", a.key, a.dir()?.display());
            return Ok(());
        }
        if let Some(st) = child.try_wait()? {
            bail!("the hotkey process exited ({st}): {}", new_log_line(log_start));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill(); // so the report is true: a daemon that came up late must not outlive "it failed"
    bail!("the hotkey did not come up within 2 s: {}", new_log_line(log_start))
}

/// The newest log line written since `from` bytes, so a start-up failure can say why. A line from
/// an earlier run is never shown as this run's reason.
fn new_log_line(from: u64) -> String {
    let p = log_path();
    let text = std::fs::read(&p).ok().filter(|b| b.len() as u64 >= from).map(|b| String::from_utf8_lossy(&b[from as usize..]).into_owned());
    match text.as_deref().and_then(|t| t.lines().last()) {
        Some(l) => format!("{l} (log: {})", p.display()),
        None => format!("it wrote nothing to its log ({})", p.display()),
    }
}

fn running() -> Option<HWND> {
    unsafe { FindWindowExW(Some(HWND_MESSAGE), None, CLASS, None).ok() }
}

fn autostart(a: &HotkeyArgs, t: Toggle) -> Result<()> {
    let ok = match t {
        Toggle::On => {
            let exe = std::env::current_exe()?;
            let cmd = std::iter::once(quote_arg(&exe.display().to_string())).chain(a.passthrough().iter().map(|s| quote_arg(s))).collect::<Vec<_>>().join(" ");
            std::process::Command::new("reg").args(["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &cmd, "/f"]).output()?
        }
        Toggle::Off => {
            // Ask first rather than parse reg.exe's (localised) complaint about a missing value.
            if !std::process::Command::new("reg").args(["query", RUN_KEY, "/v", RUN_VALUE]).output()?.status.success() {
                println!("autostart was already off");
                return Ok(());
            }
            std::process::Command::new("reg").args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]).output()?
        }
    };
    if !ok.status.success() {
        bail!("reg.exe failed: {}", String::from_utf8_lossy(&ok.stderr).trim());
    }
    println!("autostart {}", if matches!(t, Toggle::On) { "on: the hotkey starts at logon" } else { "off" });
    Ok(())
}

fn daemon(a: &HotkeyArgs) -> Result<()> {
    let r = daemon_inner(a);
    if let Err(e) = &r {
        log(&format!("fatal: {e:#}"));
    }
    r
}

fn daemon_inner(a: &HotkeyArgs) -> Result<()> {
    if running().is_some() {
        bail!("another hotkey process is already running");
    }
    let (mods, vk) = parse_key(&a.key)?;
    let dir = a.dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    unsafe {
        unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            if m == WM_CLOSE {
                // Stop taking presses, but keep the window: it is what `--stop` and `running()`
                // look for, and it owns the clipboard until the last frame is saved.
                PostQuitMessage(0);
                return LRESULT(0);
            }
            if m == WM_DESTROY {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            DefWindowProcW(h, m, w, l)
        }
        let hinst = GetModuleHandleW(None)?;
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: hinst.into(), lpszClassName: CLASS, ..Default::default() });
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, w!(""), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None)?;
        if let Err(e) = RegisterHotKey(Some(hwnd), 1, mods, vk) {
            bail!("cannot register {}: {e} (another program already owns that combination; pick another with --key)", a.key);
        }
        log(&format!("hotkey {} registered; saving to {}", a.key, dir.display()));
        let _rdp = if a.no_rdp_hook { None } else { Some(rdp::start(hwnd, mods, vk)) };
        if let Some(n) = a.prune_days {
            prune(&dir, n);
        }
        // stderr is the null device, so a panic would end the hotkey with no trace at all.
        std::panic::set_hook(Box::new(|i| log(&format!("panic: {i}"))));
        let mut cap: Option<Capturer> = None;
        // The worker does the slow half of each press, in order, so the last press wins the
        // clipboard. The daemon window owns the clipboard: clearing it sends a synchronous message
        // to the previous owner, so the owner's thread must keep pumping messages. The daemon
        // thread does, because it never blocks on the worker: past QUEUE_CAP + 1 unsaved frames a
        // press is dropped and logged (a frame is a whole screen, about 27 MB at 4K, so the
        // backlog is bounded), and at shutdown it pumps while it waits for the worker.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Grab>(QUEUE_CAP + 1);
        let worker = {
            let (dir, hw, prune_days) = (dir.clone(), hwnd.0 as isize, a.prune_days);
            std::thread::spawn(move || {
                let hw = HWND(hw as *mut _);
                for g in rx {
                    // Everything for one job is inside the catch, so even a panic in the log or
                    // the prune cannot end the worker and leave the daemon with nobody to save.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let err = match process(g, &dir, hw) {
                            Ok(()) => None,
                            Err(e) => Some(format!("{e:#}")),
                        };
                        if let Some(e) = err {
                            log(&format!("saving a screenshot failed: {e}"));
                            let _ = MessageBeep(MB_ICONHAND);
                        }
                        if let Some(n) = prune_days {
                            prune(&dir, n);
                        }
                    }));
                    if r.is_err() {
                        log("saving a screenshot panicked (see the panic line above)");
                    }
                    PENDING.fetch_sub(1, Ordering::Relaxed);
                }
            })
        };
        let mut msg = MSG::default();
        loop {
            match GetMessageW(&mut msg, None, 0, 0).0 {
                0 => break, // WM_QUIT: --stop
                -1 => {
                    log("message loop error (GetMessageW failed); stopping");
                    break;
                }
                _ => {}
            }
            if msg.message == WM_HOTKEY && PENDING.load(Ordering::Relaxed) > QUEUE_CAP {
                // The worker is that far behind: take no frame rather than block this thread, which
                // must keep pumping messages (--stop, the remote-desktop focus watcher).
                log(&format!("press dropped: {} screenshots are still being saved", PENDING.load(Ordering::Relaxed)));
                let _ = MessageBeep(MB_ICONHAND);
            } else if msg.message == WM_HOTKEY {
                // A panic in one press must not take the hotkey down with it: treat it as a failed press.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| grab(a, &mut cap)));
                let err = match r {
                    Ok(Ok(g)) => {
                        PENDING.fetch_add(1, Ordering::Relaxed);
                        if let Err(e) = tx.try_send(g) {
                            PENDING.fetch_sub(1, Ordering::Relaxed);
                            log(&format!("could not hand the frame to the saving worker ({e})"));
                            let _ = MessageBeep(MB_ICONHAND);
                        }
                        None
                    }
                    Ok(Err(e)) => Some(format!("{e:#}")),
                    Err(_) => Some("panicked (see the panic line above)".to_string()),
                };
                if let Some(e) = err {
                    log(&format!("capture failed: {e}"));
                    cap = None; // a lost D3D device heals by being recreated
                    let _ = MessageBeep(MB_ICONHAND);
                }
            } else {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = UnregisterHotKey(Some(hwnd), 1);
        // The remote-desktop hook goes first: while we drain the backlog it would swallow the key
        // inside a remote window and give the user neither a screenshot nor the key.
        drop(_rdp);
        // Let the screenshots already taken finish saving before the process exits. Keep pumping
        // meanwhile: the worker's clipboard writes send a message to this window. Bounded: clearing
        // the clipboard waits on the window of whichever program owned it, and a hung one would
        // otherwise keep this process (and `running()`) alive for good. Longer than `--stop`'s own
        // 5 s wait, so a slow but healthy save still reports truthfully.
        drop(tx);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            let mut m = MSG::default();
            while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            log(&format!("giving up on {} unsaved screenshot(s) after 20 s (the clipboard owner may be hung)", PENDING.load(Ordering::Relaxed)));
        }
        let _ = DestroyWindow(hwnd);
    }
    log("stopped");
    Ok(())
}

/// Frames that may wait for the worker (one more is being saved), and how many are unsaved now.
const QUEUE_CAP: usize = 6;
static PENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
use std::sync::atomic::Ordering;

/// What one press takes the instant the key goes down. Everything slow (measuring the text,
/// encoding, files, clipboard) happens afterwards on the worker, so a burst of presses grabs
/// one frame per press, when it was pressed, instead of each waiting for the one before it.
struct Grab {
    frame: Frame,
    index: usize,
    name: String,
    rect: capture::Rect,
    title: String,
    stamp: String, // local time of the grab: what the file name says, however long saving waits
}

/// The fast half of a press: the focused display's frame, plus the flash and the sound.
fn grab(a: &HotkeyArgs, cap: &mut Option<Capturer>) -> Result<Grab> {
    let stamp = local_stamp();
    let ds = capture::displays()?;
    let (d, title) = focused(&ds)?;
    // After sleep or a display change the kept D3D device can be dead: retry once with a fresh one.
    let frame = match cap.as_mut().map(|c| crate::capture_display(c, d)) {
        Some(Ok(f)) => f,
        first => {
            if let Some(Err(e)) = first {
                log(&format!("grab failed ({e:#}); retrying once with a new D3D device"));
            }
            *cap = Some(Capturer::new(false)?);
            crate::capture_display(cap.as_mut().unwrap(), d)?
        }
    };
    if frame.rgb.iter().all(|&b| b == 0) {
        // shoot() warns on stderr; here stderr is the null device, so the log is the only place.
        log(&format!("warning: {} captured as an entirely black frame (protected content, or a capture failure); saving it anyway", d.name));
    }
    // The frame is in memory, so the feedback cannot end up in the screenshot.
    if !a.no_flash {
        let _ = crate::fx::flash(vec![d.rect], 120);
    }
    if !a.no_sound {
        let _ = crate::fx::shutter();
    }
    Ok(Grab { frame, index: d.index, name: d.name.clone(), rect: d.rect, title, stamp })
}

/// The slow half, in order on the worker: size it for Claude, save, clipboard.
fn process(g: Grab, dir: &Path, hwnd: HWND) -> Result<()> {
    // The folder can vanish while the daemon runs (deleted, renamed, a drive that came back).
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let ds = capture::displays()?;
    let shot = <crate::Cli as clap::Parser>::parse_from(["winshot"]).shot;
    let name = g.name.clone();
    let job = crate::Job { frame: g.frame, displays: vec![g.index], rect: g.rect, label: g.name, suffix: None };
    let tiles = crate::for_claude(&shot, &ds, &crate::calibrate::load(), &job)?;
    let names = file_names(&unique_stem_at(dir, &g.stamp, &g.title), tiles.len());
    let mut paths = Vec::new();
    let mut png = Vec::new();
    for (t, file) in tiles.iter().zip(&names) {
        let p = dir.join(file);
        let enc = encode::encode(t, Format::Png)?;
        std::fs::write(&p, &enc.bytes).with_context(|| format!("writing {}", p.display()))?;
        png = enc.bytes;
        paths.push(p);
    }
    // One image fits one clipboard slot; several tiles go as files and paths only.
    let image = (tiles.len() == 1).then(|| (&tiles[0], png.as_slice()));
    set_clipboard(hwnd, image, &paths)?;
    log(&format!("{name} -> {} file(s), first {}", paths.len(), paths[0].display()));
    Ok(())
}

/// The display holding the focused window (else the one under the mouse), and that window's title.
fn focused(ds: &[Display]) -> Result<(&Display, String)> {
    let (hmon, title) = unsafe {
        let fg = GetForegroundWindow();
        if !fg.0.is_null() && !IsIconic(fg).as_bool() {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(fg, &mut buf).max(0) as usize;
            (MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST), String::from_utf16_lossy(&buf[..n]))
        } else {
            let mut p = POINT::default();
            GetCursorPos(&mut p)?;
            (MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST), String::new())
        }
    };
    let d = ds.iter().find(|d| d.hmon == hmon).or_else(|| ds.iter().find(|d| d.primary)).context("no display")?;
    Ok((d, title))
}

/// `2026-10-09_10_48_16_417-<title>`: the file stem for one press, never one already in `dir`.
/// Names are built by appending, never with `with_extension`, which would cut a title like
/// "report.docx - Word" at its last dot.
fn local_stamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02}_{:02}_{:02}_{:02}_{:03}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
}

fn unique_stem_at(dir: &Path, stamp: &str, title: &str) -> String {
    let stamp = stamp.to_string();
    let title = sanitize(title);
    let name = if title.is_empty() { stamp } else { format!("{stamp}-{title}") };
    let mut stem = name.clone();
    let mut n = 1;
    while stem_taken(dir, &stem) {
        n += 1;
        stem = format!("{name}~{n}");
    }
    stem
}

/// Is `<stem>.png` or `<stem>-1of3.png` (any tiles) already in `dir`?
fn stem_taken(dir: &Path, stem: &str) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().strip_prefix(stem).is_some_and(|rest| rest.starts_with('.') || rest.starts_with('-'))))
}

/// The file names for `n` images from one press: `<stem>.png`, or `<stem>-1of3.png`, ...
fn file_names(stem: &str, n: usize) -> Vec<String> {
    if n == 1 {
        vec![format!("{stem}.png")]
    } else {
        (1..=n).map(|i| format!("{stem}-{i}of{n}.png")).collect()
    }
}

/// A window title as a file-name part: no characters Windows forbids, at most 60 chars.
fn sanitize(title: &str) -> String {
    let s: String = title.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).take(60).collect();
    s.trim_matches(|c: char| c == ' ' || c == '.' || c == '_').to_string()
}

/// Delete winshot's own files (date-stamped PNGs) older than `days`; nothing else.
fn prune(dir: &Path, days: u32) {
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(days as u64 * 86_400)) else { return };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if is_ours(&e.file_name().to_string_lossy()) && e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < cutoff) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Is this a file winshot wrote: the full `2026-10-09_10_48_16_417` stamp, then `-title`,
/// `~n` or nothing, ending in `.png`? (Anything looser would delete someone's `2024-trip.png`.)
fn is_ours(name: &str) -> bool {
    const STAMP: &[u8] = b"dddd-dd-dd_dd_dd_dd_ddd";
    let b = name.as_bytes();
    b.len() > STAMP.len()
        && name.ends_with(".png")
        && b.iter().zip(STAMP).all(|(c, p)| if *p == b'd' { c.is_ascii_digit() } else { c == p })
        && matches!(b[STAMP.len()], b'-' | b'~' | b'.')
}

// ---------------------------------------------------------------- remote desktop

/// While a remote-desktop client has focus it installs its own low-level keyboard hook, and a
/// low-level hook runs BEFORE `RegisterHotKey` is applied (the newest hook first), so the client
/// forwards the key to the remote machine and the hotkey never fires. The way round it is a
/// low-level hook of our own, installed after the client's so that ours runs first. To keep the
/// hook's reach small it exists only while such a client is the foreground window.
///
/// The hook lives on a thread of its own that does nothing but pump messages. A low-level hook
/// only runs while its thread is inside `GetMessageW`; on the daemon thread, which is busy for
/// about a second per screenshot, every key typed meanwhile would wait out the system's hook
/// timeout (300 ms by default) and Windows can drop a hook that times out.
///
/// The registered hotkey stays in place the whole time. When our hook is first in line it takes
/// the key and swallows it, so the registered one never sees it; when it is not (an elevated
/// window, a client that hooks later), the registered hotkey still gets its chance.
mod rdp {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::Relaxed};
    use std::sync::Mutex;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};

    /// Programs that carry a remote session in a window of their own (matched by lowercase
    /// executable name; the Windows App's name is a guess).
    pub const CLIENTS: &[&str] = &["mstsc.exe", "msrdc.exe", "msrdcw.exe", "windows365.exe", "wfica32.exe", "cdviewer.exe", "vmware-view.exe"];
    /// A key-down this soon after our last swallowed one, with the key still marked held, is an
    /// auto-repeat. (Repeats come every 30 ms or so after a first delay of 250, 500, 750 or 1000
    /// ms, Windows' keyboard-delay setting. Too large costs nothing: a real second press has a
    /// key-up before it, which clears the held state.)
    const REPEAT_MS: u32 = 1500;

    static DAEMON: AtomicIsize = AtomicIsize::new(0); // the daemon's message window
    static MODS: AtomicU32 = AtomicU32::new(0);
    static VK: AtomicU32 = AtomicU32::new(0);
    static HOOK: AtomicIsize = AtomicIsize::new(0);
    static IN_RDP: AtomicBool = AtomicBool::new(false);
    static HELD: AtomicBool = AtomicBool::new(false); // our key is down and being swallowed
    static LAST_DOWN: AtomicU32 = AtomicU32::new(0); // event time of the last swallowed key-down
    static SEEN: AtomicU32 = AtomicU32::new(0); // key events the hook saw while installed
    static FIRED: AtomicU32 = AtomicU32::new(0); // presses it took
    static REPEATS: AtomicU32 = AtomicU32::new(0); // auto-repeats it swallowed
    static THREAD: Mutex<Option<(u32, std::thread::JoinHandle<()>)>> = Mutex::new(None);
    static ACTIVE: AtomicIsize = AtomicIsize::new(0); // the client window the hook thread was started for

    /// Keeps the focus watcher alive; dropping it removes the watcher and the hook thread.
    pub struct Guard(HWINEVENTHOOK);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                let _ = UnhookWinEvent(self.0);
            }
            stop_thread();
        }
    }

    pub fn start(hwnd: HWND, mods: HOT_KEY_MODIFIERS, vk: u32) -> Guard {
        DAEMON.store(hwnd.0 as isize, Relaxed);
        MODS.store(mods.0, Relaxed);
        VK.store(vk, Relaxed);
        let g = unsafe { SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_foreground_event), 0, 0, WINEVENT_OUTOFCONTEXT) };
        if g.0.is_null() {
            log("warning: could not watch the foreground window; inside a remote-desktop window the key may not work");
        } else {
            on_foreground();
        }
        Guard(g)
    }

    // The event can sit in the queue for a second during a press, by when its window may be gone:
    // ask Windows who has focus now instead of trusting the event.
    unsafe extern "system" fn on_foreground_event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
        on_foreground();
    }

    fn client_exe(hwnd: HWND) -> Option<String> {
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let exe = capture::exe_name(pid).to_ascii_lowercase();
        CLIENTS.contains(&exe.as_str()).then_some(exe)
    }

    fn on_foreground() {
        let fg = unsafe { GetForegroundWindow() };
        match client_exe(fg) {
            Some(exe) => {
                if !IN_RDP.swap(true, Relaxed) {
                    log(&format!("{exe} has focus: the key is also read by a keyboard hook while it does"));
                }
                // A client installs its hook when it gets focus, so a new window starts the
                // install over. The same window again (a child window or the connection bar
                // taking focus and handing it back) must not tear a working hook down.
                if ACTIVE.swap(fg.0 as isize, Relaxed) != fg.0 as isize || thread_lock().is_none() {
                    start_thread();
                }
            }
            None => {
                ACTIVE.store(0, Relaxed);
                if IN_RDP.swap(false, Relaxed) {
                    stop_thread();
                }
            }
        }
    }

    fn start_thread() {
        stop_thread();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || unsafe {
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE); // creates this thread's queue
            let _ = tx.send(GetCurrentThreadId());
            // Install once the client has had time to install its own hook, and again in case it
            // installed (or re-installed) later. Why two is a guess: only the second was seen to matter.
            let t1 = SetTimer(None, 0, 400, None);
            let t2 = SetTimer(None, 0, 1500, None);
            if t1 == 0 || t2 == 0 {
                log("warning: SetTimer failed; installing the keyboard hook at once");
                install_hook();
            }
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                if msg.message == WM_TIMER && msg.wParam.0 != 0 && (msg.wParam.0 == t1 || msg.wParam.0 == t2) {
                    let _ = KillTimer(None, msg.wParam.0);
                    install_hook();
                } else {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            let _ = KillTimer(None, t1);
            let _ = KillTimer(None, t2);
            remove_hook(true);
        });
        // Blocking: the thread's first two calls cannot hang, and Err means it died. A timeout
        // here would drop the handle of a thread that could still start and nothing could stop.
        match rx.recv() {
            Ok(tid) => *thread_lock() = Some((tid, handle)),
            Err(_) => log("warning: the keyboard-hook thread died at start"),
        }
    }

    /// A poisoned lock is still a usable lock here: the daemon must not panic over it.
    fn thread_lock() -> std::sync::MutexGuard<'static, Option<(u32, std::thread::JoinHandle<()>)>> {
        THREAD.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stop_thread() {
        let taken = thread_lock().take(); // the guard ends here, not across the join
        if let Some((tid, handle)) = taken {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = handle.join(); // it only pumps messages, so this returns at once
        }
    }

    /// Only ever called on the hook thread.
    fn install_hook() {
        remove_hook(false); // a new hook is the newest, so it runs first
        unsafe {
            let hinst = GetModuleHandleW(None).ok();
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hinst.map(Into::into), 0) {
                Ok(h) => HOOK.store(h.0 as isize, Relaxed),
                Err(e) => log(&format!("could not install the keyboard hook ({e}); the registered hotkey still works where the client allows it")),
            }
        }
    }

    /// Only ever called on the hook thread.
    fn remove_hook(clear_held: bool) {
        let h = HOOK.swap(0, Relaxed);
        if h != 0 {
            unsafe {
                let _ = UnhookWindowsHookEx(HHOOK(h as *mut _));
            }
            // How many key events reached the hook (never which keys): tells "installed but the
            // client still got there first" (0) from "installed and working".
            log(&format!(
                "keyboard hook removed; it saw {} key event(s), took {} press(es), swallowed {} auto-repeat(s)",
                SEEN.swap(0, Relaxed),
                FIRED.swap(0, Relaxed),
                REPEATS.swap(0, Relaxed)
            ));
        }
        if clear_held {
            HELD.store(false, Relaxed); // a replaced hook keeps it: the key may be down right now
        }
    }

    #[derive(Debug, PartialEq)]
    pub enum Verdict {
        Repeat, // the key we took is still down and repeating: swallow, fire nothing
        Fire,   // a new press of the hotkey: swallow and fire
        Pass,   // not ours
    }

    /// `mods_ok` is a closure so the modifier state is read only when it matters.
    pub fn decide(held: bool, last: u32, now: u32, mods_ok: impl FnOnce() -> bool) -> Verdict {
        if held && now.wrapping_sub(last) < REPEAT_MS {
            Verdict::Repeat
        } else if mods_ok() {
            Verdict::Fire
        } else {
            Verdict::Pass
        }
    }

    /// Do the modifiers held right now equal exactly the ones the hotkey asks for?
    fn mods_match(want: u32) -> bool {
        let down = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0;
        let bit = |held: bool, m: HOT_KEY_MODIFIERS| if held { m.0 } else { 0 };
        let have = bit(down(VK_MENU), MOD_ALT) | bit(down(VK_CONTROL), MOD_CONTROL) | bit(down(VK_SHIFT), MOD_SHIFT) | bit(down(VK_LWIN) || down(VK_RWIN), MOD_WIN);
        have == want & !MOD_NOREPEAT.0
    }

    /// Runs inside the system's input path: no logging, no allocation, nothing that can panic.
    unsafe extern "system" fn hook_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        if code >= 0 {
            SEEN.fetch_add(1, Relaxed);
            let k = &*(l.0 as *const KBDLLHOOKSTRUCT);
            if k.vkCode == VK.load(Relaxed) {
                let (down, up) = (matches!(w.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN), matches!(w.0 as u32, WM_KEYUP | WM_SYSKEYUP));
                if down {
                    match decide(HELD.load(Relaxed), LAST_DOWN.load(Relaxed), k.time, || mods_match(MODS.load(Relaxed))) {
                        Verdict::Repeat => {
                            REPEATS.fetch_add(1, Relaxed);
                            LAST_DOWN.store(k.time, Relaxed);
                            return LRESULT(1);
                        }
                        Verdict::Fire => {
                            FIRED.fetch_add(1, Relaxed);
                            HELD.store(true, Relaxed);
                            LAST_DOWN.store(k.time, Relaxed);
                            // The remote machine must not get the key as well.
                            let _ = PostMessageW(Some(HWND(DAEMON.load(Relaxed) as *mut _)), WM_HOTKEY, WPARAM(1), LPARAM(0));
                            return LRESULT(1);
                        }
                        Verdict::Pass => {}
                    }
                } else if up && HELD.swap(false, Relaxed) {
                    return LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, code, w, l)
    }

    #[cfg(test)]
    pub fn clients_are_lowercase_exe_names() -> bool {
        CLIENTS.iter().all(|c| c.ends_with(".exe") && *c == c.to_ascii_lowercase())
    }
}

// ---------------------------------------------------------------- clipboard

fn set_clipboard(hwnd: HWND, image: Option<(&Frame, &[u8])>, paths: &[PathBuf]) -> Result<()> {
    unsafe {
        // Another app can hold the clipboard for a moment; wait it out.
        let mut opened = false;
        for _ in 0..20 {
            if OpenClipboard(Some(hwnd)).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if !opened {
            bail!("the clipboard stayed locked by another program");
        }
        // Closed on drop, so a panic while filling (caught per press) cannot leave every other
        // program locked out of the clipboard.
        struct Open;
        impl Drop for Open {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseClipboard();
                }
            }
        }
        let _open = Open;
        fill_clipboard(image, paths)
    }
}

unsafe fn fill_clipboard(image: Option<(&Frame, &[u8])>, paths: &[PathBuf]) -> Result<()> {
    // Build every payload first, so what can fail cheaply fails before the old contents are cleared.
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
    let mut items: Vec<(u32, Vec<u8>)> = Vec::new();
    if let Some((f, png)) = image {
        let png_format = RegisterClipboardFormatW(w!("PNG"));
        if png_format == 0 {
            bail!("RegisterClipboardFormatW(\"PNG\") failed");
        }
        items.push((CF_DIB, dib(f)));
        items.push((png_format, png.to_vec()));
    }
    items.push((CF_UNICODETEXT, wide(&paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\r\n"))));
    items.push((CF_HDROP, hdrop(paths)));
    EmptyClipboard()?;
    for (format, bytes) in &items {
        if let Err(e) = put(*format, bytes) {
            let _ = EmptyClipboard(); // empty, never half-filled
            return Err(e);
        }
    }
    Ok(())
}

unsafe fn put(format: u32, bytes: &[u8]) -> Result<()> {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
    let p = GlobalLock(h);
    if p.is_null() {
        let _ = GlobalFree(Some(h));
        bail!("GlobalLock failed");
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    let _ = GlobalUnlock(h);
    // On success the clipboard owns the memory.
    if let Err(e) = SetClipboardData(format, Some(HANDLE(h.0))) {
        let _ = GlobalFree(Some(h));
        return Err(e.into());
    }
    Ok(())
}

/// CF_DIB: BITMAPINFOHEADER + 24-bit BGR rows, bottom-up, each padded to 4 bytes.
fn dib(f: &Frame) -> Vec<u8> {
    let stride = (f.w as usize * 3).div_ceil(4) * 4;
    let size = stride * f.h as usize;
    let mut v = Vec::with_capacity(40 + size);
    for x in [40u32, f.w, f.h, 1 | (24 << 16), 0, size as u32, 2835, 2835, 0, 0] {
        v.extend_from_slice(&x.to_le_bytes()); // biWidth/biHeight positive = bottom-up
    }
    for y in (0..f.h as usize).rev() {
        let row = &f.rgb[y * f.w as usize * 3..(y + 1) * f.w as usize * 3];
        for px in row.chunks_exact(3) {
            v.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        v.resize(v.len() + stride - f.w as usize * 3, 0);
    }
    v
}

/// CF_HDROP: DROPFILES (20 bytes, wide names) then each path NUL-terminated, then one more NUL.
fn hdrop(paths: &[PathBuf]) -> Vec<u8> {
    let mut v = Vec::new();
    for x in [20u32, 0, 0, 0, 1] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    for p in paths {
        v.extend(p.display().to_string().encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes));
    }
    v.extend_from_slice(&[0, 0]);
    v
}

// ---------------------------------------------------------------- keys and log

/// "ctrl+`" -> modifiers (with MOD_NOREPEAT) and a virtual-key code on the current layout.
fn parse_key(s: &str) -> Result<(HOT_KEY_MODIFIERS, u32)> {
    let (mut mods, mut vk) = (MOD_NOREPEAT, None);
    for part in s.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" => mods |= MOD_WIN,
            k => {
                if vk.replace(key_vk(k)?).is_some() {
                    bail!("--key {s:?} names more than one key");
                }
            }
        }
    }
    let vk = vk.with_context(|| format!("--key {s:?} has no key, only modifiers"))?;
    if mods == MOD_NOREPEAT {
        bail!("--key {s:?} has no modifier; a bare key would be taken from every program");
    }
    Ok((mods, vk))
}

fn key_vk(k: &str) -> Result<u32> {
    let named = match k {
        "printscreen" | "prtsc" => Some(0x2C),
        "space" => Some(0x20),
        "pause" => Some(0x13),
        _ => k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=24).contains(n)).map(|n| 0x6F + n),
    };
    if let Some(v) = named {
        return Ok(v);
    }
    let mut it = k.chars();
    let (Some(c), None) = (it.next(), it.next()) else { bail!("unknown key {k:?}") };
    if c.len_utf16() != 1 {
        bail!("{c:?} is outside the basic plane; Windows names keys by one UTF-16 unit");
    }
    let r = unsafe { VkKeyScanW(c as u16) };
    if r == -1 {
        bail!("this keyboard layout has no key for {c:?}");
    }
    if (r >> 8) & 0xFF != 0 {
        bail!("{c:?} needs a modifier on this layout; name that modifier explicitly and use the base key");
    }
    Ok((r & 0xFF) as u32)
}

fn log_path() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("winshot").join("hotkey.log")
}

fn log(msg: &str) {
    let p = log_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    // Keep the log small: start over once it passes 256 KiB.
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::remove_file(&p);
    }
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    // One buffer, one write: two threads now log, and several small writes could interleave lines.
    let line = format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} {msg}\n", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_and_bare_keys_are_refused() {
        // The backtick key is layout-dependent (VK_OEM_3 on a US keyboard): compare with Windows' own answer.
        match parse_key("ctrl+`") {
            Ok((m, vk)) => {
                assert_eq!(m, MOD_CONTROL | MOD_NOREPEAT);
                assert_eq!(vk, unsafe { VkKeyScanW('`' as u16) } as u32 & 0xFF);
            }
            Err(e) => assert!(e.to_string().contains("layout"), "{e}"), // a layout with no plain backtick key
        }
        let (m, vk) = parse_key("Ctrl + Shift + s").unwrap();
        assert_eq!((m, vk), (MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, 0x53));
        assert_eq!(parse_key("alt+f12").unwrap().1, 0x7B);
        assert!(parse_key("s").is_err(), "no modifier");
        assert!(parse_key("ctrl+").is_err(), "no key");
        assert!(parse_key("ctrl+a+b").is_err(), "two keys");
        assert!(parse_key("ctrl+\u{1F600}").is_err(), "outside the basic plane");
    }

    #[test]
    fn titles_become_safe_file_name_parts() {
        assert_eq!(sanitize(r#"a<b>c:d"e/f\g|h?i*j"#), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("  untitled • - Sublime Text (UNREGISTERED) "), "untitled • - Sublime Text (UNREGISTERED)");
        assert_eq!(sanitize("x".repeat(100).as_str()).len(), 60);
        assert_eq!(sanitize("..."), "");
    }

    #[test]
    fn names_keep_dots_in_titles_and_never_reuse_a_stem() {
        let dir = std::env::temp_dir().join(format!("winshot-stem-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(file_names("s-report.docx - Word", 1), ["s-report.docx - Word.png"]);
        assert_eq!(file_names("s", 3), ["s-1of3.png", "s-2of3.png", "s-3of3.png"]);
        let a = unique_stem_at(&dir, &local_stamp(), "report.docx - Word");
        assert!(a.ends_with("-report.docx - Word"), "{a}");
        // A stem already used, as one image or as tiles, counts as taken; a longer name does not.
        assert!(!stem_taken(&dir, &a));
        std::fs::write(dir.join(format!("{a}.png")), b"x").unwrap();
        assert!(stem_taken(&dir, &a), "single image");
        std::fs::remove_file(dir.join(format!("{a}.png"))).unwrap();
        std::fs::write(dir.join(format!("{a}-2of3.png")), b"x").unwrap();
        assert!(stem_taken(&dir, &a), "tiles");
        assert!(!stem_taken(&dir, &format!("{a}~2")), "~2 is a different stem");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dib_is_bottom_up_bgr_padded_to_4() {
        let f = Frame { w: 1, h: 2, rgb: vec![1, 2, 3, 4, 5, 6] }; // top pixel RGB 1,2,3; bottom 4,5,6
        let d = dib(&f);
        assert_eq!(&d[..4], &40u32.to_le_bytes());
        assert_eq!(d.len(), 40 + 2 * 4);
        assert_eq!(&d[40..44], &[6, 5, 4, 0], "bottom row first, BGR, padded");
        assert_eq!(&d[44..48], &[3, 2, 1, 0]);
    }

    #[test]
    fn hdrop_lists_wide_nul_separated_paths() {
        let d = hdrop(&[PathBuf::from(r"C:\a.png"), PathBuf::from(r"C:\b.png")]);
        assert_eq!(&d[..4], &20u32.to_le_bytes());
        assert_eq!(&d[16..20], &1u32.to_le_bytes(), "fWide");
        assert_eq!(d.len(), 20 + (8 + 1) * 2 * 2 + 2);
        assert_eq!(&d[d.len() - 4..], &[0, 0, 0, 0], "list ends with a double NUL");
    }

    #[test]
    fn prune_only_removes_old_winshot_pngs() {
        let dir = std::env::temp_dir().join(format!("winshot-prune-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = SystemTime::now() - Duration::from_secs(10 * 86_400);
        // Decoys are long, old, start like a date or number, and must survive.
        for (name, aged) in [
            ("2026-10-01_10_00_00_000-x.png", true),
            ("2026-10-01_10_00_00_000-x-1of3.png", true),
            ("2026-10-01_10_00_00_000~2.png", true),
            ("2026-10-09_10_00_00_000-x.png", false),
            ("notes.png", true),
            ("2026-10-01_10_00_00_000-x.txt", true),
            ("2024-vacation-photo-beach-sunset.png", true),
            ("1080-wallpaper-collection-final.png", true),
            ("2026-10-01-holiday-photo-album-1.png", true),
            ("2026-10-01_10_00_00_000x-nearly.png", true),
            ("２０２６-10-01_10_00_00_000-wide-digits.png", true),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            if aged {
                std::fs::File::options().write(true).open(&p).unwrap().set_modified(old).unwrap();
            }
        }
        prune(&dir, 5);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        let mut want = vec![
            "2026-10-01_10_00_00_000-x.txt",
            "2026-10-09_10_00_00_000-x.png",
            "notes.png",
            "2024-vacation-photo-beach-sunset.png",
            "1080-wallpaper-collection-final.png",
            "2026-10-01-holiday-photo-album-1.png",
            "2026-10-01_10_00_00_000x-nearly.png",
            "２０２６-10-01_10_00_00_000-wide-digits.png",
        ];
        want.sort();
        assert_eq!(left, want, "only the three old winshot-named PNGs go");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn same_millisecond_presses_get_distinct_stems() {
        let dir = std::env::temp_dir().join(format!("winshot-collide-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stamp = "2026-10-09_10_48_16_417";
        let first = unique_stem_at(&dir, stamp, "Doc.docx - Word");
        assert_eq!(first, format!("{stamp}-Doc.docx - Word"));
        std::fs::write(dir.join(format!("{first}.png")), b"x").unwrap();
        let second = unique_stem_at(&dir, stamp, "Doc.docx - Word");
        assert_eq!(second, format!("{first}~2"));
        std::fs::write(dir.join(format!("{second}-1of2.png")), b"x").unwrap(); // taken as tiles
        assert_eq!(unique_stem_at(&dir, stamp, "Doc.docx - Word"), format!("{first}~3"));
        assert_eq!(unique_stem_at(&dir, stamp, ""), format!("{stamp}~2"), "files with this stamp exist, so even a bare stamp is taken (conservative)");
        assert_eq!(unique_stem_at(&dir, "2026-10-09_10_48_16_418", ""), "2026-10-09_10_48_16_418", "no title, no suffix when free");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn options_survive_the_relaunch_and_the_autostart_entry() {
        use clap::Parser;
        let parsed = crate::Cli::try_parse_from(["winshot", "hotkey", "--no-rdp-hook", "--no-sound", "--key", "ctrl+f9", "--prune-days", "7"]).unwrap();
        let Some(crate::Cmd::Hotkey(a)) = parsed.cmd else { panic!("not the hotkey command") };
        let p = a.passthrough();
        for want in ["--no-rdp-hook", "--no-sound", "ctrl+f9", "--prune-days", "7"] {
            assert!(p.iter().any(|x| x == want), "{want} lost: {p:?}");
        }
        assert!(!p.iter().any(|x| x == "--no-flash"), "an option nobody gave appeared: {p:?}");
    }

    #[test]
    fn a_press_is_told_from_its_auto_repeat() {
        use rdp::{decide, Verdict};
        let yes = || true;
        let no = || false;
        assert_eq!(decide(false, 0, 100, yes), Verdict::Fire, "first press");
        assert_eq!(decide(true, 100, 130, yes), Verdict::Repeat, "repeat 30 ms later");
        assert_eq!(decide(true, 100, 1100, yes), Verdict::Repeat, "first repeat after the longest keyboard delay (1 s)");
        assert_eq!(decide(true, 100, 3100, yes), Verdict::Fire, "key-up lost, slow second press");
        assert_eq!(decide(false, 0, 5, no), Verdict::Pass, "other modifiers held");
        assert_eq!(decide(true, u32::MAX - 10, 20, yes), Verdict::Repeat, "the 49-day clock wrap");
        assert_eq!(decide(true, 100, 100, no), Verdict::Repeat, "a repeat is swallowed even if the modifiers were let go");
    }

    #[test]
    fn remote_desktop_clients_are_matched_by_lowercase_exe_name() {
        assert!(rdp::clients_are_lowercase_exe_names());
        assert!(rdp::CLIENTS.contains(&"mstsc.exe") && rdp::CLIENTS.contains(&"windows365.exe"));
    }

    #[test]
    fn is_ours_needs_the_full_stamp() {
        assert!(is_ours("2026-10-09_10_48_16_417.png"));
        assert!(is_ours("2026-10-09_10_48_16_417-a title.png"));
        assert!(is_ours("2026-10-09_10_48_16_417~2-1of3.png"));
        assert!(!is_ours("2026-10-09_10_48_16_417"), "no extension");
        assert!(!is_ours("2026-10-09_10_48_16_41-x.png"), "short stamp");
        assert!(!is_ours("2026-10-09_10_48_16_417x.png"), "stamp must end at - ~ or .");
        assert!(!is_ours("2026-10-09.png"));
    }

    #[test]
    fn arguments_survive_a_command_line_round_trip() {
        assert_eq!(quote_arg("ctrl+`"), "\"ctrl+`\"");
        assert_eq!(quote_arg(r"C:\My Shots\"), r#""C:\My Shots\\""#, "a trailing backslash must not eat the closing quote");
        assert_eq!(quote_arg(r"C:\a\b"), r#""C:\a\b""#, "inner backslashes stay single");
        assert_eq!(quote_arg("a\"b"), r#""a\"b""#);
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#, "backslashes before a quote are doubled, plus one to escape it");
    }
}
