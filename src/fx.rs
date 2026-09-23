//! Capture feedback: a white flash over what was captured, and a shutter click.
//! Both are started only AFTER the frame is in memory (see main.rs), otherwise
//! the flash would be in the screenshot.

use crate::capture::Rect;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, HBRUSH, WHITE_BRUSH};
use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// White flash over each rect, fading 180 -> 0 alpha over `ms` (120 for a capture).
pub fn flash(rects: Vec<Rect>, ms: u32) -> JoinHandle<()> {
    thread::spawn(move || unsafe {
        unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            DefWindowProcW(h, m, w, l)
        }
        let Ok(hinst) = GetModuleHandleW(None) else { return };
        let class = w!("winshot_flash");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
            ..Default::default()
        });
        // Click-through, never activates, not in the taskbar.
        let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
        let hwnds: Vec<HWND> = rects
            .iter()
            .filter_map(|r| {
                CreateWindowExW(ex, class, w!(""), WS_POPUP, r.x, r.y, r.w as i32, r.h as i32, None, None, Some(hinst.into()), None).ok()
            })
            .collect();
        let steps = (ms / 10).max(1); // one alpha step per ~10 ms frame
        for step in 0..=steps {
            for &h in &hwnds {
                let _ = SetLayeredWindowAttributes(h, COLORREF(0), (180 * (steps - step) / steps) as u8, LWA_ALPHA);
                if step == 0 {
                    let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
                }
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            thread::sleep(Duration::from_millis(10));
        }
        for h in hwnds {
            let _ = DestroyWindow(h);
        }
    })
}

/// Play the shutter click; the thread returns whether Windows reported it played.
pub fn shutter() -> JoinHandle<bool> {
    thread::spawn(|| {
        let wav = shutter_wav();
        unsafe { PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_SYNC | SND_NODEFAULT).as_bool() }
    })
}

/// An originally synthesized two-transient mechanical click (no product's audio
/// asset is used or reproduced). Deterministic: fixed-seed noise, so the sound
/// is reproducible from these parameters alone.
pub fn shutter_wav() -> Vec<u8> {
    const SR: f32 = 44_100.0;
    let mut out = vec![0f32; (SR * 0.14) as usize];
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    };
    // (start s, burst s, decay tau s, amplitude, band-pass centre Hz): mirror up, then
    // a quieter, lower-centroid mirror down ~75 ms later; both inside 2-6 kHz.
    for (t0, len, tau, amp, fc) in [(0.0, 0.004, 0.0012, 1.0, 3800.0), (0.075, 0.005, 0.0018, 0.7, 2600.0)] {
        let (b0, a1, a2, a0) = {
            let w0 = 2.0 * std::f32::consts::PI * fc / SR;
            let alpha = w0.sin() / (2.0 * 0.9);
            (alpha, -2.0 * w0.cos(), 1.0 - alpha, 1.0 + alpha)
        };
        let (mut x1, mut x2, mut y1, mut y2) = (0f32, 0f32, 0f32, 0f32);
        let start = (t0 * SR) as usize;
        for k in 0..((len + 0.03) * SR) as usize {
            let t = k as f32 / SR;
            let x = if t < len { noise() * (-t / tau).exp() } else { 0.0 };
            let y = (b0 * x - b0 * x2 - a1 * y1 - a2 * y2) / a0;
            (x2, x1, y2, y1) = (x1, x, y1, y);
            if let Some(s) = out.get_mut(start + k) {
                *s += amp * y;
            }
        }
    }
    let peak = out.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-9);
    let pcm: Vec<u8> = out.iter().flat_map(|s| ((s / peak * 0.8 * 32767.0) as i16).to_le_bytes()).collect();
    let mut wav = Vec::with_capacity(44 + pcm.len());
    for chunk in [
        &b"RIFF"[..],
        &(36 + pcm.len() as u32).to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &1u16.to_le_bytes(), // PCM
        &1u16.to_le_bytes(), // mono
        &(SR as u32).to_le_bytes(),
        &(SR as u32 * 2).to_le_bytes(),
        &2u16.to_le_bytes(),
        &16u16.to_le_bytes(),
        b"data",
        &(pcm.len() as u32).to_le_bytes(),
        &pcm,
    ] {
        wav.extend_from_slice(chunk);
    }
    wav
}

#[cfg(test)]
mod tests {
    /// Two transients ~75 ms apart, ~140 ms long, energy centred in 2-6 kHz.
    #[test]
    fn shutter_is_two_band_limited_transients() {
        let wav = super::shutter_wav();
        let s: Vec<f32> = wav[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32).collect();
        assert!((0.13..0.15).contains(&(s.len() as f32 / 44_100.0)));
        let peak = |a: usize, b: usize| s[a..b].iter().fold(0f32, |m, x| m.max(x.abs()));
        let ms = |t: f32| (t * 44.1) as usize;
        assert!(peak(0, ms(10.0)) > 20_000.0, "transient A");
        assert!(peak(ms(40.0), ms(70.0)) < 500.0, "silent gap");
        let b = peak(ms(74.0), ms(90.0));
        assert!(b > 5_000.0 && b < peak(0, ms(10.0)), "quieter transient B");
        // Spectral centroid via a coarse DFT over 0-11 kHz.
        let (mut num, mut den) = (0f64, 0f64);
        for f in (200..11_000).step_by(200) {
            let w = 2.0 * std::f64::consts::PI * f as f64 / 44_100.0;
            let (re, im) = s.iter().enumerate().fold((0f64, 0f64), |(r, i), (n, &x)| (r + x as f64 * (w * n as f64).cos(), i + x as f64 * (w * n as f64).sin()));
            let p = re * re + im * im;
            num += f as f64 * p;
            den += p;
        }
        let centroid = num / den;
        assert!((2_000.0..6_000.0).contains(&centroid), "centroid {centroid} Hz");
    }
}
