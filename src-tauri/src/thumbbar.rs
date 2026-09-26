//! Windows taskbar thumbnail toolbar: ⏮ ⏯ ⏭ buttons under the hover preview.
//! Everything here must run on the main (UI) thread.

use std::{cell::RefCell, sync::OnceLock};

use tauri::{AppHandle, Manager, Window};
use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS},
        System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
        UI::{
            Shell::{
                DefSubclassProc, ITaskbarList3, SetWindowSubclass, TaskbarList, THBF_ENABLED, THBN_CLICKED,
                THB_FLAGS, THB_ICON, THB_TOOLTIP, THUMBBUTTON,
            },
            WindowsAndMessaging::{
                CreateIconIndirect, GetSystemMetrics, RegisterWindowMessageW, HICON, ICONINFO, SM_CXSMICON,
                WM_COMMAND,
            },
        },
    },
};

use crate::{run_action, Action};

const ID_PREV: u32 = 1;
const ID_TOGGLE: u32 = 2;
const ID_NEXT: u32 = 3;

static APP: OnceLock<AppHandle> = OnceLock::new();
static BUTTON_CREATED_MSG: OnceLock<u32> = OnceLock::new();

struct Icons {
    prev: HICON,
    play: HICON,
    pause: HICON,
    next: HICON,
}

struct Bar {
    hwnd: HWND,
    taskbar: ITaskbarList3,
    icons: Icons,
    playing: bool,
}

thread_local! {
    static BAR: RefCell<Option<Bar>> = const { RefCell::new(None) };
}

pub fn install(window: &Window) {
    let _ = APP.set(window.app_handle().clone());
    let Ok(hwnd) = window.hwnd() else { return };
    unsafe {
        let _ = BUTTON_CREATED_MSG.set(RegisterWindowMessageW(w!("TaskbarButtonCreated")));
        let Ok(taskbar) = CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER) else {
            return;
        };
        if taskbar.HrInit().is_err() {
            return;
        }
        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        let icons = Icons {
            prev: make_icon(size, |x, y| next_shape(1.0 - x, y)),
            play: make_icon(size, play_shape),
            pause: make_icon(size, pause_shape),
            next: make_icon(size, next_shape),
        };
        BAR.with(|b| *b.borrow_mut() = Some(Bar { hwnd, taskbar, icons, playing: false }));
        let _ = SetWindowSubclass(hwnd, Some(subclass_proc), 1, 0);
    }
    // The taskbar button may already exist; if not, TaskbarButtonCreated will add them.
    add_buttons();
}

/// Swap the middle button between play and pause.
pub fn set_playing(playing: bool) {
    BAR.with(|b| {
        let mut b = b.borrow_mut();
        let Some(bar) = b.as_mut() else { return };
        if bar.playing == playing {
            return;
        }
        bar.playing = playing;
        let btn = toggle_button(&bar.icons, playing);
        unsafe {
            let _ = bar.taskbar.ThumbBarUpdateButtons(bar.hwnd, &[btn]);
        }
    });
}

fn add_buttons() {
    BAR.with(|b| {
        let b = b.borrow();
        let Some(bar) = b.as_ref() else { return };
        let buttons = [
            button(ID_PREV, bar.icons.prev, "Previous"),
            toggle_button(&bar.icons, bar.playing),
            button(ID_NEXT, bar.icons.next, "Next"),
        ];
        unsafe {
            let _ = bar.taskbar.ThumbBarAddButtons(bar.hwnd, &buttons);
        }
    });
}

fn toggle_button(icons: &Icons, playing: bool) -> THUMBBUTTON {
    if playing {
        button(ID_TOGGLE, icons.pause, "Pause")
    } else {
        button(ID_TOGGLE, icons.play, "Play")
    }
}

fn button(id: u32, icon: HICON, tip: &str) -> THUMBBUTTON {
    let mut b = THUMBBUTTON {
        dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
        iId: id,
        hIcon: icon,
        dwFlags: THBF_ENABLED,
        ..Default::default()
    };
    for (dst, src) in b.szTip.iter_mut().zip(tip.encode_utf16()) {
        *dst = src;
    }
    b
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if Some(&msg) == BUTTON_CREATED_MSG.get() {
        // Sent each time the taskbar button is (re)created, e.g. after hiding to tray.
        add_buttons();
    } else if msg == WM_COMMAND && ((wparam.0 >> 16) & 0xffff) as u32 == THBN_CLICKED {
        let action = match (wparam.0 & 0xffff) as u32 {
            ID_PREV => Some(Action::Prev),
            ID_TOGGLE => Some(Action::Toggle),
            ID_NEXT => Some(Action::Next),
            _ => None,
        };
        if let (Some(app), Some(a)) = (APP.get(), action) {
            run_action(app, a);
        }
        return LRESULT(0);
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

// ---------- icon drawing (white glyphs, 4x4 supersampled) ----------

fn in_triangle(px: f32, py: f32, [a, b, c]: [(f32, f32); 3]) -> bool {
    let cross = |(x1, y1): (f32, f32), (x2, y2): (f32, f32)| (x2 - x1) * (py - y1) - (y2 - y1) * (px - x1);
    let (d1, d2, d3) = (cross(a, b), cross(b, c), cross(c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

fn in_rect(x: f32, y: f32, x0: f32, x1: f32, y0: f32, y1: f32) -> bool {
    x >= x0 && x <= x1 && y >= y0 && y <= y1
}

fn play_shape(x: f32, y: f32) -> bool {
    in_triangle(x, y, [(0.25, 0.14), (0.25, 0.86), (0.86, 0.5)])
}

fn pause_shape(x: f32, y: f32) -> bool {
    in_rect(x, y, 0.22, 0.42, 0.15, 0.85) || in_rect(x, y, 0.58, 0.78, 0.15, 0.85)
}

fn next_shape(x: f32, y: f32) -> bool {
    in_triangle(x, y, [(0.16, 0.18), (0.16, 0.82), (0.66, 0.5)]) || in_rect(x, y, 0.70, 0.84, 0.18, 0.82)
}

fn make_icon(size: i32, shape: impl Fn(f32, f32) -> bool) -> HICON {
    const SS: i32 = 4;
    let n = size as usize;
    let mut pixels = vec![0u32; n * n];
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    hits += shape(x, y) as u32;
                }
            }
            let a = hits * 255 / (SS * SS) as u32;
            // Premultiplied white BGRA.
            pixels[py as usize * n + px as usize] = (a << 24) | (a << 16) | (a << 8) | a;
        }
    }

    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // top-down
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return HICON::default();
        };
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u32, pixels.len());
        let mask = CreateBitmap(size, size, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO { fIcon: true.into(), hbmMask: mask, hbmColor: color, ..Default::default() })
            .unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}
