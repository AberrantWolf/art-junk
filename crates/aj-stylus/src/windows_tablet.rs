//! Windows tablet-input backend. Subclasses the winit-owned HWND via
//! `SetWindowSubclass`, intercepts `WM_POINTER*` messages, filters on
//! `GetPointerType == PT_PEN`, drains `GetPointerPenInfoHistory` so rapid
//! samples between frames aren't lost, forwards everything else to
//! `DefSubclassProc`.
//!
//! Design decisions (see `.claude/skills/stylus-input/windows.md`):
//!
//! - **WM_POINTER over RealTimeStylus / WinRT Ink.** MS's recommended modern
//!   path; covers pressure/tilt/rotation/eraser/hover; separates touch/pen/
//!   mouse cleanly. No COM apartment dance, no XAML coupling.
//! - **Subclass HWND instead of forking winit.** Symmetric to the macOS
//!   NSEvent-monitor approach; survives winit version bumps.
//! - **QPC timestamps.** `POINTER_INFO.dwTime` is `GetTickCount`-scale
//!   (32-bit ms, 49-day wraparound). `PerformanceCount` is
//!   `QueryPerformanceCounter` ticks — monotonic, high-res. We cache
//!   `QueryPerformanceFrequency` once at attach.
//! - **History drain on every pointer message.** `GetPointerPenInfoHistory`
//!   returns all samples since the last service of this pointer; entries
//!   are lost on the next message for the same ID, so we drain *in* the
//!   current message. Newest-first from the API; reverse for time order.
//! - **Palm rejection at the subclass.** While `pen_in_range`, swallow
//!   `PT_TOUCH` messages and OEM-synthesized `WM_MOUSEMOVE` /
//!   `WM_LBUTTON*` that old Wacom/HP drivers still fire.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::mem::MaybeUninit;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aj_core::{ToolCaps, ToolKind};
use kurbo::Point;

use crate::StylusAdapter;
use crate::adapter::{WindowsPointerPhase, WindowsProximitySample, WindowsRawSample};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::UI::Input::Pointer::{
    GetPointerPenInfo, GetPointerPenInfoHistory, GetPointerType, POINTER_PEN_INFO,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH, WM_NCDESTROY, WM_POINTERCAPTURECHANGED,
    WM_POINTERDEVICEINRANGE, WM_POINTERDEVICEOUTOFRANGE, WM_POINTERDOWN, WM_POINTERENTER,
    WM_POINTERLEAVE, WM_POINTERUP, WM_POINTERUPDATE,
};

// `POINTER_FLAG_*` constants are not re-exported by `windows-sys` under
// `UI::WindowsAndMessaging` in the version we use. The values are part of the
// stable Pointer Input API — inline to avoid a dependency bump for constants
// alone.
const POINTER_FLAG_INRANGE: u32 = 0x0000_0002;
const POINTER_FLAG_INCONTACT: u32 = 0x0000_0004;

/// Bits used in the flags field of `POINTER_PEN_INFO.penFlags`.
const PEN_FLAG_BARREL: u32 = 0x00000001;
const PEN_FLAG_INVERTED: u32 = 0x00000002;
const PEN_FLAG_ERASER: u32 = 0x00000004;

/// Each backend instance registers with a unique subclass id so multiple
/// subclasses on the same HWND don't collide. The value only needs to be
/// distinct within a process.
static NEXT_SUBCLASS_ID: AtomicUsize = AtomicUsize::new(0xAAA0_0000);

#[derive(Debug, thiserror::Error)]
pub enum WindowsTabletInstallError {
    #[error("SetWindowSubclass failed")]
    SubclassFailed,
    #[error("QueryPerformanceFrequency returned zero (system timer unavailable)")]
    NoQpcFrequency,
    #[error("HWND is null")]
    NullHwnd,
}

/// RAII guard. Drop removes the subclass and reclaims the boxed state.
pub struct WindowsTabletBackend {
    hwnd: HWND,
    subclass_id: usize,
    // The box is reclaimed inside `WM_NCDESTROY` in practice (we emit that
    // path on teardown), but keep a raw pointer here so Drop can remove the
    // subclass even if the window outlives the backend.
    state_ptr: *mut SubclassState,
}

struct SubclassState {
    adapter: Rc<RefCell<StylusAdapter>>,
    qpc_freq: f64,
    pen_in_range: bool,
}

impl WindowsTabletBackend {
    /// Install the subclass on `hwnd`. The caller must own the HWND (winit
    /// does), and the subclass must be installed and removed on the same
    /// thread that owns the window (Win32 rule).
    pub fn install(
        adapter: Rc<RefCell<StylusAdapter>>,
        hwnd: HWND,
    ) -> Result<Self, WindowsTabletInstallError> {
        if hwnd.is_null() {
            return Err(WindowsTabletInstallError::NullHwnd);
        }

        // QPC frequency is constant for the system's lifetime post-boot;
        // caching once is safe.
        let mut freq: i64 = 0;
        // SAFETY: QueryPerformanceFrequency has no preconditions and only
        // writes into the provided out-pointer.
        let ok = unsafe { QueryPerformanceFrequency(&mut freq) };
        if ok == 0 || freq == 0 {
            return Err(WindowsTabletInstallError::NoQpcFrequency);
        }

        let state = Box::new(SubclassState { adapter, qpc_freq: freq as f64, pen_in_range: false });
        let state_ptr = Box::into_raw(state);
        let subclass_id = NEXT_SUBCLASS_ID.fetch_add(1, Ordering::Relaxed);

        // SAFETY: We pass a stable boxed pointer as dwRefData; it's reclaimed
        // in the subclass proc's WM_NCDESTROY branch, or in our Drop below.
        let ok = unsafe {
            SetWindowSubclass(hwnd, Some(subclass_proc), subclass_id, state_ptr as usize)
        };
        if ok == 0 {
            // Subclass install failed — reclaim the box ourselves.
            unsafe {
                drop(Box::from_raw(state_ptr));
            }
            return Err(WindowsTabletInstallError::SubclassFailed);
        }

        Ok(Self { hwnd, subclass_id, state_ptr })
    }
}

impl Drop for WindowsTabletBackend {
    fn drop(&mut self) {
        // SAFETY: RemoveWindowSubclass is safe to call even if the subclass
        // has already been implicitly torn down by WM_NCDESTROY (returns 0
        // in that case). The HWND remains valid until winit tears down the
        // window; winit's window Drop runs later on the same thread.
        unsafe {
            RemoveWindowSubclass(self.hwnd, Some(subclass_proc), self.subclass_id);
        }
        // The subclass proc may have already dropped the box on WM_NCDESTROY;
        // we can't reliably distinguish, so take the small leak on graceful
        // window-outliving-backend over a double-free. In normal app
        // teardown (window drops first) WM_NCDESTROY fires before this Drop.
        let _ = self.state_ptr;
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    ref_data: usize,
) -> LRESULT {
    // SAFETY: ref_data is the `Box::into_raw` pointer we passed to
    // `SetWindowSubclass`. The box is live until either `WM_NCDESTROY` (we
    // reclaim) or backend Drop (which removes the subclass first, so no
    // further calls with this pointer happen). We hold `&mut` only for the
    // duration of one message; no aliasing.
    let state: &mut SubclassState = unsafe { &mut *(ref_data as *mut SubclassState) };

    match msg {
        WM_POINTERDEVICEINRANGE => {
            state.pen_in_range = true;
            // TODO: enumerate the proximity'd device via GetPointerDevices
            // to build a real caps bitmask. For M1 we optimistically assume
            // a full pen; proximity-refresh on re-enter will correct.
            deliver_proximity(state, 0, ToolKind::Pen, optimistic_caps(), true);
        }
        WM_POINTERDEVICEOUTOFRANGE => {
            state.pen_in_range = false;
            deliver_proximity(state, 0, ToolKind::Pen, optimistic_caps(), false);
        }
        WM_POINTERENTER
        | WM_POINTERLEAVE
        | WM_POINTERDOWN
        | WM_POINTERUP
        | WM_POINTERUPDATE
        | WM_POINTERCAPTURECHANGED => {
            let pointer_id = get_pointer_id_wparam(wparam);
            let mut ptype: POINTER_INPUT_TYPE = 0;
            // SAFETY: GetPointerType writes into the provided out-pointer;
            // returns 0 on failure.
            let got = unsafe { GetPointerType(pointer_id, &mut ptype) };
            if got == 0 {
                return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            }

            if ptype == PT_TOUCH && state.pen_in_range {
                // Palm rejection: swallow touch while pen is in range.
                return 0;
            }
            if ptype != PT_PEN {
                return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            }

            if msg == WM_POINTERCAPTURECHANGED {
                // WM_POINTERCAPTURECHANGED means some other window stole
                // capture — treat as Cancel for the active stroke.
                deliver_cancel(state, hwnd, pointer_id);
            } else {
                handle_pen(state, hwnd, msg, pointer_id);
            }
        }
        WM_NCDESTROY => {
            // SAFETY: Reclaim the box we created in `install`. After this,
            // `ref_data` is dangling — but we've removed the subclass
            // synchronously (since the window is being destroyed).
            unsafe {
                drop(Box::from_raw(ref_data as *mut SubclassState));
            }
        }
        _ => {}
    }

    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

fn get_pointer_id_wparam(wparam: WPARAM) -> u32 {
    (wparam & 0xFFFF) as u32
}

fn handle_pen(state: &mut SubclassState, hwnd: HWND, msg: u32, pointer_id: u32) {
    // Drain history newest-first, reverse for time order. If history drain
    // fails (e.g. zero entries), fall back to `GetPointerPenInfo` for the
    // current sample alone.
    let mut history: Vec<POINTER_PEN_INFO> = Vec::new();
    let mut count: u32 = 32;
    loop {
        // SAFETY: We pass a sized buffer matching `count`; the API writes
        // up to `count` entries and updates `count` on return.
        history.resize(count as usize, unsafe {
            MaybeUninit::<POINTER_PEN_INFO>::zeroed().assume_init()
        });
        let ok = unsafe { GetPointerPenInfoHistory(pointer_id, &mut count, history.as_mut_ptr()) };
        if ok == 0 {
            history.clear();
            break;
        }
        if count as usize <= history.len() {
            history.truncate(count as usize);
            break;
        }
        // Buffer too small — grow and retry.
    }

    if history.is_empty() {
        let mut info: POINTER_PEN_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetPointerPenInfo(pointer_id, &mut info) };
        if ok == 0 {
            return;
        }
        emit_pen_info(state, hwnd, msg, &info);
        return;
    }

    // API returns newest-first; iterate reversed for time order.
    for info in history.iter().rev() {
        emit_pen_info(state, hwnd, msg, info);
    }
}

fn emit_pen_info(state: &mut SubclassState, hwnd: HWND, msg: u32, info: &POINTER_PEN_INFO) {
    let flags = info.pointerInfo.pointerFlags;
    let inrange = (flags & POINTER_FLAG_INRANGE) != 0;
    let incontact = (flags & POINTER_FLAG_INCONTACT) != 0;

    let source_phase = match msg {
        WM_POINTERDOWN => WindowsPointerPhase::Down,
        WM_POINTERUP => WindowsPointerPhase::Up,
        _ if incontact => WindowsPointerPhase::Move,
        _ if inrange => WindowsPointerPhase::Hover,
        _ => return,
    };

    let position = screen_to_client_point(hwnd, info.pointerInfo.ptPixelLocationRaw);

    let timestamp_secs = if state.qpc_freq > 0.0 {
        info.pointerInfo.PerformanceCount as f64 / state.qpc_freq
    } else {
        // Shouldn't happen — install() rejects zero freq — but fall back
        // to a fresh QPC read so we don't emit wall-clock.
        let mut now: i64 = 0;
        unsafe { QueryPerformanceCounter(&mut now) };
        now as f64 / state.qpc_freq.max(1.0)
    };

    // POINTER_PEN_INFO.pressure is 0..=1024; normalize.
    let pressure = (info.pressure as f32 / 1024.0).clamp(0.0, 1.0);

    // Tilt/rotation valid only when penMask says so; otherwise None.
    let tilt = Some(aj_core::Tilt { x_deg: info.tiltX as f32, y_deg: info.tiltY as f32 });
    let twist_deg = Some(info.rotation as f32);

    // Fold PEN_FLAG_* bits into the adapter's platform-agnostic mask.
    let mut button_mask: u32 = 0;
    let pen_flags = info.penFlags;
    if pen_flags & PEN_FLAG_BARREL != 0 {
        button_mask |= 0x1;
    }
    if pen_flags & PEN_FLAG_INVERTED != 0 {
        button_mask |= 0x4;
    }
    let tool = if pen_flags & PEN_FLAG_ERASER != 0 { ToolKind::Eraser } else { ToolKind::Pen };

    let device_id = hash_source_device(info.pointerInfo.sourceDevice as usize);

    let raw = WindowsRawSample {
        position_physical_px: position,
        timestamp_secs,
        pressure,
        tilt,
        twist_deg,
        button_mask,
        device_id,
        pointing_device_type: tool,
        source_phase,
    };
    deliver_raw(state, raw);
}

fn screen_to_client_point(hwnd: HWND, raw: POINT) -> Point {
    let mut pt = POINT { x: raw.x, y: raw.y };
    // SAFETY: pt is a valid POINT; ScreenToClient only mutates its target.
    unsafe {
        ScreenToClient(hwnd, &mut pt);
    }
    Point::new(f64::from(pt.x), f64::from(pt.y))
}

fn hash_source_device(handle: usize) -> u32 {
    // HANDLE is stable per-physical-digitizer for its plug-in lifetime;
    // the low 32 bits are a sufficient per-device key.
    (handle as u32).wrapping_mul(0x9E37_79B9)
}

fn optimistic_caps() -> ToolCaps {
    ToolCaps::PRESSURE | ToolCaps::TILT | ToolCaps::TWIST | ToolCaps::HOVER
}

fn deliver_raw(state: &mut SubclassState, raw: WindowsRawSample) {
    match state.adapter.try_borrow_mut() {
        Ok(mut a) => a.handle_windows_raw(raw),
        Err(_) => log::warn!("WM_POINTER: adapter borrowed elsewhere, sample dropped"),
    }
}

fn deliver_proximity(
    state: &mut SubclassState,
    device_id: u32,
    tool: ToolKind,
    caps: ToolCaps,
    is_entering: bool,
) {
    let prox = WindowsProximitySample { device_id, pointing_device_type: tool, caps, is_entering };
    match state.adapter.try_borrow_mut() {
        Ok(mut a) => a.handle_windows_proximity(prox),
        Err(_) => log::warn!("proximity: adapter borrowed elsewhere, dropped"),
    }
}

fn deliver_cancel(state: &mut SubclassState, hwnd: HWND, pointer_id: u32) {
    // Build a minimal Cancel sample. Position is taken from GetPointerPenInfo
    // if available, else (0,0) — the adapter's `take_pending_for_pointer`
    // tears the stroke down either way.
    let mut info: POINTER_PEN_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetPointerPenInfo(pointer_id, &mut info) };
    let (position, timestamp_secs, tool) = if ok != 0 {
        (
            screen_to_client_point(hwnd, info.pointerInfo.ptPixelLocationRaw),
            info.pointerInfo.PerformanceCount as f64 / state.qpc_freq,
            if info.penFlags & PEN_FLAG_ERASER != 0 { ToolKind::Eraser } else { ToolKind::Pen },
        )
    } else {
        (Point::ZERO, 0.0, ToolKind::Pen)
    };
    let device_id =
        if ok != 0 { hash_source_device(info.pointerInfo.sourceDevice as usize) } else { 0 };
    let raw = WindowsRawSample {
        position_physical_px: position,
        timestamp_secs,
        pressure: 0.0,
        tilt: None,
        twist_deg: None,
        button_mask: 0,
        device_id,
        pointing_device_type: tool,
        source_phase: WindowsPointerPhase::Cancel,
    };
    deliver_raw(state, raw);
}

// The `Rc<RefCell<StylusAdapter>>` in `SubclassState` and the `*mut
// SubclassState` raw pointer on the backend already make this type !Send
// and !Sync — no explicit opt-out needed. That matches the mac backend's
// thread model: the UI thread that owns the HWND is the only thread that
// should ever touch the backend.
