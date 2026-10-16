//! Spike for the overlay spec's Slice 0 (#383): does a window with
//! `WDA_EXCLUDEFROMCAPTURE` stay out of Wingman's own screenshot?
//!
//! Run by hand: `cargo run --release --example capture_exclusion_spike`
//!
//! For each window kind (layered/click-through/topmost/no-activate, and a
//! plain non-layered popup), with and without the affinity flag, it shows a
//! small solid-colour window in the primary monitor's top-left corner for
//! about 300 ms, grabs the monitor with `xcap::Monitor::capture_image` (the
//! exact call `src/capture.rs` makes) and `PrintWindow`, and reports whether
//! the window colour is present where the window sits.
//!
//! The window class name is test-only (AGENTS.md rule 9).

use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC,
    DeleteObject, GetDC, GetDIBits, GetMonitorInfoW, MonitorFromPoint, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS, HDC, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowDisplayAffinity,
    PeekMessageW, RegisterClassW, SetWindowDisplayAffinity, ShowWindow, TranslateMessage,
    UnregisterClassW, UpdateLayeredWindow, MSG, PM_REMOVE, SW_SHOWNOACTIVATE, ULW_ALPHA,
    WDA_EXCLUDEFROMCAPTURE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const CLASS: PCWSTR = w!("WingmanSpikeCaptureExclusionTestOnly");
const SIZE_PX: i32 = 64;
/// R, G, B. Deliberately not a colour any desktop uses.
const RGB: (u8, u8, u8) = (12, 233, 71);

#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: HWND, hdc: HDC, flags: u32) -> i32;
}

unsafe extern "system" fn wnd_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn pump(ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn is_spike(px: [u8; 3]) -> bool {
    let d = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 3;
    d(px[0], RGB.0) && d(px[1], RGB.1) && d(px[2], RGB.2)
}

/// Count spike-coloured pixels in the window rect of an RGBA buffer, and the
/// colour at the rect's centre.
fn scan_rgba(buf: &[u8], w_px: usize, h_px: usize, x0: usize, y0: usize) -> (usize, [u8; 3]) {
    let mut hits = 0;
    let mut centre = [0u8; 3];
    let s = SIZE_PX as usize;
    for y in y0..(y0 + s).min(h_px) {
        for x in x0..(x0 + s).min(w_px) {
            let i = (y * w_px + x) * 4;
            let Some(p) = buf.get(i..i + 3) else { continue };
            let px = [p[0], p[1], p[2]];
            if is_spike(px) {
                hits += 1;
            }
            if y == y0 + s / 2 && x == x0 + s / 2 {
                centre = px;
            }
        }
    }
    (hits, centre)
}

struct Outcome {
    affinity_set: String,
    affinity_read: u32,
    xcap_hits: usize,
    xcap_centre: [u8; 3],
    pw_hits: Option<usize>,
    pw_centre: [u8; 3],
}

fn bmi() -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: SIZE_PX,
            biHeight: -SIZE_PX,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn run_case(layered: bool, flag: bool, left: i32, top: i32) -> Outcome {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let brush = CreateSolidBrush(COLORREF(
            (RGB.0 as u32) | ((RGB.1 as u32) << 8) | ((RGB.2 as u32) << 16),
        ));
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            hbrBackground: brush,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let ex = if layered {
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
        } else {
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
        };
        let hwnd = CreateWindowExW(
            ex,
            CLASS,
            w!(""),
            WS_POPUP,
            left,
            top,
            SIZE_PX,
            SIZE_PX,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .unwrap();

        let affinity_set = if flag {
            match SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("FAILED {e}"),
            }
        } else {
            "not called".to_string()
        };
        let mut aff = 0u32;
        let _ = GetWindowDisplayAffinity(hwnd, &mut aff);

        if layered {
            // 32bpp premultiplied DIB, solid, alpha 255, as the overlay would.
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let dib =
                CreateDIBSection(Some(mem), &bmi(), DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let px =
                std::slice::from_raw_parts_mut(bits as *mut u8, (SIZE_PX * SIZE_PX * 4) as usize);
            for c in px.chunks_exact_mut(4) {
                c.copy_from_slice(&[RGB.2, RGB.1, RGB.0, 255]);
            }
            let old = SelectObject(mem, dib.into());
            let pos = POINT { x: left, y: top };
            let size = SIZE { cx: SIZE_PX, cy: SIZE_PX };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                hwnd,
                Some(screen),
                Some(&pos),
                Some(&size),
                Some(mem),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            SelectObject(mem, old);
            let _ = DeleteObject(dib.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        pump(300);

        // Wingman's capture call (src/capture.rs): xcap Monitor::capture_image.
        let (mut xcap_hits, mut xcap_centre) = (0, [0u8; 3]);
        let monitors = xcap::Monitor::all().unwrap();
        if let Some(m) = monitors.iter().find(|m| m.is_primary().unwrap_or(false)) {
            let img = m.capture_image().unwrap();
            let (w_, h_) = (img.width() as usize, img.height() as usize);
            let mx = m.x().unwrap_or(0);
            let my = m.y().unwrap_or(0);
            (xcap_hits, xcap_centre) =
                scan_rgba(img.as_raw(), w_, h_, (left - mx) as usize, (top - my) as usize);
        }

        // PrintWindow with PW_RENDERFULLCONTENT (2).
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, SIZE_PX, SIZE_PX);
        let old = SelectObject(mem, bmp.into());
        let pw_ok = PrintWindow(hwnd, mem, 2);
        let mut bgra = vec![0u8; (SIZE_PX * SIZE_PX * 4) as usize];
        let mut info = bmi();
        GetDIBits(
            mem,
            bmp,
            0,
            SIZE_PX as u32,
            Some(bgra.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        for c in bgra.chunks_exact_mut(4) {
            c.swap(0, 2);
        }
        let (pw_hits, pw_centre) = if pw_ok != 0 {
            let (h, c) = scan_rgba(&bgra, SIZE_PX as usize, SIZE_PX as usize, 0, 0);
            (Some(h), c)
        } else {
            (None, [0; 3])
        };
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);

        let _ = DestroyWindow(hwnd);
        pump(30);
        let _ = UnregisterClassW(CLASS, Some(hinst.into()));
        let _ = DeleteObject(brush.into());
        Outcome { affinity_set, affinity_read: aff, xcap_hits, xcap_centre, pw_hits, pw_centre }
    }
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi =
            MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (left, top) = (mi.rcMonitor.left + 8, mi.rcMonitor.top + 8);
        println!(
            "window {SIZE_PX}x{SIZE_PX} at ({left},{top}), colour {RGB:?}, {} px",
            SIZE_PX * SIZE_PX
        );
        for (layered, flag) in [(true, false), (true, true), (false, false), (false, true)] {
            let o = run_case(layered, flag, left, top);
            let pw = o.pw_hits.map_or("PrintWindow failed".to_string(), |h| h.to_string());
            println!(
                "layered={layered:<5} flag={flag:<5} affinity_call={} (read back {}) xcap_hits={} centre={:?} PrintWindow_hits={pw} centre={:?}",
                o.affinity_set, o.affinity_read, o.xcap_hits, o.xcap_centre, o.pw_centre
            );
        }
    }
}
