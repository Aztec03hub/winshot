//! Display / window enumeration and Windows.Graphics.Capture one-shot grabs.
//! Every coordinate here is PHYSICAL pixels: the process is PER_MONITOR_AWARE_V2
//! (asserted in `assert_dpi_aware`), so Windows reports the real desktop.

use anyhow::{anyhow, bail, Context, Result};
use std::sync::mpsc;
use std::time::Duration;
use windows::core::{Interface, BOOL, PWSTR};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// A captured frame, tightly packed RGB8.
#[derive(Clone)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<u8>,
}

impl Frame {
    pub fn black(w: u32, h: u32) -> Frame {
        Frame { w, h, rgb: vec![0; w as usize * h as usize * 3] }
    }
    /// Copy `src` into self with its top-left at (x, y); clips to self.
    pub fn blit(&mut self, src: &Frame, x: i32, y: i32) {
        for sy in 0..src.h as i32 {
            let dy = y + sy;
            if dy < 0 || dy >= self.h as i32 {
                continue;
            }
            let x0 = x.max(0);
            let x1 = (x + src.w as i32).min(self.w as i32);
            if x1 <= x0 {
                return;
            }
            let s = ((sy * src.w as i32 + (x0 - x)) * 3) as usize;
            let d = ((dy * self.w as i32 + x0) * 3) as usize;
            let n = ((x1 - x0) * 3) as usize;
            self.rgb[d..d + n].copy_from_slice(&src.rgb[s..s + n]);
        }
    }
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Frame {
        let mut out = Frame::black(w, h);
        out.blit(self, -(x as i32), -(y as i32));
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    fn from_win(r: RECT) -> Rect {
        Rect { x: r.left, y: r.top, w: (r.right - r.left).max(0) as u32, h: (r.bottom - r.top).max(0) as u32 }
    }
    pub fn right(&self) -> i32 {
        self.x + self.w as i32
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h as i32
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    pub fn union(rs: impl Iterator<Item = Rect>) -> Rect {
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for r in rs {
            x0 = x0.min(r.x);
            y0 = y0.min(r.y);
            x1 = x1.max(r.right());
            y1 = y1.max(r.bottom());
        }
        Rect { x: x0, y: y0, w: (x1 - x0) as u32, h: (y1 - y0) as u32 }
    }
}

pub struct Display {
    pub index: usize, // 1-based, enumeration order
    pub name: String, // DISPLAY1
    pub hmon: HMONITOR,
    pub rect: Rect, // physical, virtual-desktop coordinates
    pub dpi: u32,
    pub primary: bool,
    pub hdr: bool,
}

impl Display {
    pub fn scale_factor(&self) -> f64 {
        self.dpi as f64 / 96.0
    }
    pub fn logical(&self) -> (u32, u32) {
        let s = self.scale_factor();
        ((self.rect.w as f64 / s).round() as u32, (self.rect.h as f64 / s).round() as u32)
    }
}

/// Fail loudly if Windows is virtualising DPI for us. The v0 captured logical
/// pixels with no error at all, so silence is the thing to guard against.
pub fn assert_dpi_aware() -> Result<()> {
    unsafe {
        let ctx = GetThreadDpiAwarenessContext();
        if !AreDpiAwarenessContextsEqual(ctx, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).as_bool() {
            bail!(
                "process is not PER_MONITOR_AWARE_V2 (awareness={:?}); the manifest was not embedded. \
                 Captures would be silently downscaled to logical pixels.",
                GetAwarenessFromDpiAwarenessContext(ctx)
            );
        }
    }
    Ok(())
}

pub fn displays() -> Result<Vec<Display>> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        (*(lp.0 as *mut Vec<HMONITOR>)).push(h);
        true.into()
    }
    let mut hmons: Vec<HMONITOR> = Vec::new();
    unsafe { EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut hmons as *mut _ as isize)).ok()? };
    let hdr = hdr_monitors();
    let mut out = Vec::new();
    for (i, &hmon) in hmons.iter().enumerate() {
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        unsafe { GetMonitorInfoW(hmon, &mut mi as *mut _ as *mut MONITORINFO).ok()? };
        let dev = String::from_utf16_lossy(&mi.szDevice[..mi.szDevice.iter().position(|&c| c == 0).unwrap_or(32)]);
        let (mut dx, mut dy) = (0, 0);
        unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy)? };
        let rect = Rect::from_win(mi.monitorInfo.rcMonitor);
        // Cross-check against the display MODE, which is physical by definition.
        // If they disagree the process is being DPI-virtualised: refuse, loudly.
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        if !unsafe { EnumDisplaySettingsW(windows::core::PCWSTR(mi.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool() {
            bail!("{dev}: could not read the display mode, so physical resolution cannot be verified; refusing to capture");
        }
        if dm.dmPelsWidth != rect.w || dm.dmPelsHeight != rect.h {
            bail!(
                "{dev}: monitor rect {}x{} != display mode {}x{}; DPI virtualisation is active, refusing to capture",
                rect.w, rect.h, dm.dmPelsWidth, dm.dmPelsHeight
            );
        }
        out.push(Display {
            index: i + 1,
            name: dev.trim_start_matches(r"\\.\").to_string(),
            hmon,
            rect,
            dpi: dx,
            primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            hdr: hdr.contains(&hmon),
        });
    }
    Ok(out)
}

fn hdr_monitors() -> Vec<HMONITOR> {
    let mut v = Vec::new();
    let Ok(f) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return v };
    for a in 0.. {
        let Ok(ad) = (unsafe { f.EnumAdapters1(a) }) else { break };
        for o in 0.. {
            let Ok(out) = (unsafe { ad.EnumOutputs(o) }) else { break };
            if let Ok(d) = out.cast::<IDXGIOutput6>().and_then(|o6| unsafe { o6.GetDesc1() }) {
                if d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
                    v.push(d.Monitor);
                }
            }
        }
    }
    v
}

pub struct Window {
    pub hwnd: HWND,
    pub pid: u32,
    pub exe: String,
    pub title: String,
    pub minimized: bool,
    pub maximized: bool,
    pub hmon: HMONITOR,
}

/// Visible, titled, non-cloaked top-level windows in Z-order (topmost first).
pub fn windows() -> Result<Vec<Window>> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        (*(lp.0 as *mut Vec<HWND>)).push(h);
        true.into()
    }
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(cb), LPARAM(&mut hwnds as *mut _ as isize))? };
    let mut out = Vec::new();
    for hwnd in hwnds {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                continue;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4);
            if cloaked != 0 {
                continue;
            }
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut buf);
            if n == 0 {
                continue;
            }
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            out.push(Window {
                hwnd,
                pid,
                exe: exe_name(pid),
                title: String::from_utf16_lossy(&buf[..n as usize]),
                minimized: IsIconic(hwnd).as_bool(),
                maximized: IsZoomed(hwnd).as_bool(),
                hmon: MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
            });
        }
    }
    Ok(out)
}

fn exe_name(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if !ok {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..n as usize]);
        full.rsplit('\\').next().unwrap_or(&full).to_string()
    }
}

/// The window's on-screen bounds without the invisible resize border.
pub fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as _, std::mem::size_of::<RECT>() as u32) }
        .ok()
        .map(|_| Rect::from_win(r))
}

/// One D3D device reused for every grab in a run.
pub struct Capturer {
    d3d: ID3D11Device,
    ctx: ID3D11DeviceContext,
    device: IDirect3DDevice,
    pub cursor: bool,
    /// Did Windows accept IsBorderRequired=false? None until the first grab.
    pub borderless: Option<bool>,
}

impl Capturer {
    pub fn new(cursor: bool) -> Result<Capturer> {
        let mut d3d = None;
        let mut ctx = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                Some(&mut ctx),
            )?
        };
        let d3d: ID3D11Device = d3d.context("no D3D11 device")?;
        let dxgi: IDXGIDevice = d3d.cast()?;
        let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        Ok(Capturer { d3d, ctx: ctx.context("no D3D11 context")?, device, cursor, borderless: None })
    }

    pub fn monitor(&mut self, hmon: HMONITOR) -> Result<Frame> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(hmon)? };
        self.grab(&item)
    }

    pub fn window(&mut self, hwnd: HWND) -> Result<Frame> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
        self.grab(&item)
    }

    fn grab(&mut self, item: &GraphicsCaptureItem) -> Result<Frame> {
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&self.device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 1, item.Size()?)?;
        let session: GraphicsCaptureSession = pool.CreateCaptureSession(item)?;
        // Close both on every exit path: a failed window grab no longer ends the run.
        struct Closer(GraphicsCaptureSession, Direct3D11CaptureFramePool);
        impl Drop for Closer {
            fn drop(&mut self) {
                let _ = self.0.Close();
                let _ = self.1.Close();
            }
        }
        let _closer = Closer(session.clone(), pool.clone());
        session.SetIsCursorCaptureEnabled(self.cursor).context("IsCursorCaptureEnabled")?;
        self.borderless = Some(session.SetIsBorderRequired(false).is_ok());
        let (tx, rx) = mpsc::sync_channel(1);
        let token = pool.FrameArrived(&TypedEventHandler::new(move |p: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
            if let Some(p) = p.as_ref() {
                let _ = tx.try_send(p.TryGetNextFrame());
            }
            Ok(())
        }))?;
        session.StartCapture()?;
        let got = rx.recv_timeout(Duration::from_secs(2));
        let _ = pool.RemoveFrameArrived(token);
        let frame = got.map_err(|_| anyhow!("no frame from Windows.Graphics.Capture within 2 s"))??;
        let size = frame.ContentSize()?;
        let tex: ID3D11Texture2D = unsafe { frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()? };
        let out = self.read_texture(&tex, size.Width as u32, size.Height as u32);
        let _ = frame.Close();
        out
    }

    fn read_texture(&self, tex: &ID3D11Texture2D, cw: u32, ch: u32) -> Result<Frame> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let (w, h) = (cw.min(desc.Width), ch.min(desc.Height));
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { self.d3d.CreateTexture2D(&desc, None, Some(&mut staging))? };
        let staging = staging.context("no staging texture")?;
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        unsafe {
            self.ctx.CopyResource(&staging, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let src = std::slice::from_raw_parts(m.pData as *const u8, (m.RowPitch * desc.Height) as usize);
            for y in 0..h as usize {
                let row = &src[y * m.RowPitch as usize..][..w as usize * 4];
                let dst = &mut rgb[y * w as usize * 3..][..w as usize * 3];
                for (d, s) in dst.chunks_exact_mut(3).zip(row.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                }
            }
            self.ctx.Unmap(&staging, 0);
        }
        Ok(Frame { w, h, rgb })
    }
}
