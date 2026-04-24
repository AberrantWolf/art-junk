---
name: stylus-input-windows
description: Windows pen/stylus backend for stylus-junk using the Pointer Input API (WM_POINTER + GetPointerPenInfoHistory). HWND subclassed via SetWindowSubclass to coexist with winit.
---

# Windows stylus input

**Status: not started.** Target: Windows 10 1709+ / Windows 11, Surface Pen, Wacom, N-Trig.

## TL;DR

Subclass the HWND via `SetWindowSubclass` (commctrl), intercept `WM_POINTER*` messages, filter on `GetPointerType == PT_PEN`, drain `GetPointerPenInfoHistory`, forward everything else to `DefSubclassProc`. Suppress `PT_TOUCH` while any `PT_PEN` is in range. Use `POINTER_INFO.PerformanceCount` (QPC) as the monotonic timestamp.

## Why WM_POINTER

| Option | Decision |
|---|---|
| **WM_POINTER + GetPointerPenInfo** | **Use**. MS's recommended modern path; covers pressure/tilt/rotation/eraser/hover; history-drain API; separates touch/pen/mouse. |
| RealTimeStylus (COM) | Skip — MS marks legacy; COM apartment dance; background thread. |
| WinRT `InkCanvas` / `InkPresenter` | Skip — XAML-coupled; forces their rendering. |
| Raw Input (WM_INPUT + HID) | Skip — vendor-variable; re-implements pointer. |
| DirectInk / DirectManipulation | Wrong layer. |
| Tablet PC Ink Services API (v1) | Deprecated. |

## Message flow

```
WM_POINTERDEVICECHANGE        enum/removal
WM_POINTERDEVICEINRANGE       pen entered proximity
WM_POINTERENTER               pointer entered window (hover begins)
WM_POINTERUPDATE (hover)      INRANGE && !INCONTACT — hover samples
WM_POINTERDOWN                tip contact
WM_POINTERUPDATE (drawing)    INRANGE && INCONTACT — stroke samples
WM_POINTERUP                  tip lift
WM_POINTERLEAVE               pointer left window
WM_POINTERDEVICEOUTOFRANGE    pen left proximity
WM_POINTERCAPTURECHANGED      another window stole capture — treat as cancel
```

`wParam` packs pointer ID: `GET_POINTERID_WPARAM(wparam)`.

## POINTER_PEN_INFO

```c
typedef struct {
  POINTER_INFO pointerInfo;    // generic: dwTime, PerformanceCount, ptPixelLocation(Raw), pointerFlags, sourceDevice
  PEN_FLAGS    penFlags;       // BARREL / INVERTED / ERASER
  PEN_MASK     penMask;        // which of pressure/rotation/tiltX/tiltY are valid this sample
  UINT32       pressure;       // 0..=1024
  UINT32       rotation;       // 0..=359 degrees (twist)
  INT32        tiltX;          // -90..=90 degrees
  INT32        tiltY;          // -90..=90 degrees
} POINTER_PEN_INFO;
```

**Always check `penMask` before trusting a field.** Basic pens may report only `PEN_MASK_PRESSURE`.

## Draining history

`GetPointerPenInfoHistory(pointerId, &count, buf)` returns the full history of samples since the last time this pointer was serviced. Must be called **during the current message** — entries are lost on the next pointer message for that ID.

```rust
let mut count: u32 = 256;
let mut buf: Vec<POINTER_PEN_INFO> = vec![unsafe { std::mem::zeroed() }; 256];
let ok = unsafe { GetPointerPenInfoHistory(pointer_id, &mut count, buf.as_mut_ptr()) };
if ok == 0 { return; }
buf.truncate(count as usize);

// History is newest-first — reverse to emit in time order.
for ppi in buf.iter().rev() {
    emit_sample(ppi, msg);
}
```

## Pen vs touch vs mouse

`GetPointerType(pointerId, &mut ptype)` returns `PT_PEN` / `PT_TOUCH` / `PT_MOUSE` / `PT_TOUCHPAD`. Branch on ptype; `PT_MOUSE` is winit's job.

**Palm rejection**: track `pen_in_range: bool` via `WM_POINTERDEVICE{IN,OUTOF}RANGE`. While true, swallow `PT_TOUCH` messages (return 0) and OEM-synthesized `WM_MOUSEMOVE` / `WM_LBUTTONDOWN`.

## Subclassing HWND

Install via `SetWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID, dwRefData)`. The subclass runs before winit's WndProc. Return 0 only when you consciously consume; otherwise forward via `DefSubclassProc`.

```rust
unsafe extern "system" fn subclass_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
    _id: usize, ref_data: usize,
) -> LRESULT {
    let state = &mut *(ref_data as *mut WindowsStylusState);
    match msg {
        WM_POINTERENTER | WM_POINTERLEAVE
        | WM_POINTERDOWN | WM_POINTERUP | WM_POINTERUPDATE
        | WM_POINTERCAPTURECHANGED => {
            let pid = (wparam & 0xFFFF) as u32;
            let mut ptype: POINTER_INPUT_TYPE = 0;
            if GetPointerType(pid, &mut ptype) != 0 && ptype == PT_PEN {
                handle_pen(hwnd, msg, pid, state);
            } else if ptype == PT_TOUCH && state.pen_in_range {
                return 0;  // swallow touch during pen
            }
        }
        WM_POINTERDEVICEINRANGE    => state.pen_in_range = true,
        WM_POINTERDEVICEOUTOFRANGE => state.pen_in_range = false,
        WM_NCDESTROY => drop(Box::from_raw(ref_data as *mut WindowsStylusState)),
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}
```

Store owned state as `Box<WindowsStylusState>`, stash pointer in `dwRefData`, reclaim on `WM_NCDESTROY`.

## Timestamp

`POINTER_INFO.dwTime` is `GetTickCount`-scale (32-bit ms, 49-day wraparound) — **do not use**. Use `POINTER_INFO.PerformanceCount` (QPC ticks). Cache `QueryPerformanceFrequency` once:

```rust
let ts_seconds = pi.PerformanceCount as f64 / qpc_freq as f64;
```

## Coordinates

`POINTER_INFO.ptPixelLocationRaw` — screen physical pixels, sub-pixel precision (the digitizer's raw). Convert with `ScreenToClient(hwnd, &pt)`. winit's default DPI awareness is Per-Monitor v2, so physical pixels match the adapter's contract.

If you need sub-pixel precision client-relative, compute client origin once per event (`GetWindowRect`+`ClientToScreen`) and apply as an `f32` delta. For v1 the `ScreenToClient` integer path is fine.

## Axis normalization

| Windows | Our field | Conversion |
|---|---|---|
| `pressure` (0..=1024) | `pressure` (0..=1) | `p / 1024.0` |
| `tiltX/Y` (-90..=90 deg) | `tilt.x_deg/y_deg` | direct |
| `rotation` (0..=359 deg) | `twist_deg` | direct |
| — | `tangential_pressure` | always `None` — Win pointer API has no field |

## Buttons

```rust
let flags = ppi.penFlags;
let mut mask = StylusButtons::CONTACT;
let mut tool = ToolKind::Pen;
if flags & PEN_FLAG_BARREL != 0 { mask |= StylusButtons::BARREL; }
if flags & PEN_FLAG_INVERTED != 0 { mask |= StylusButtons::INVERTED; }
if flags & PEN_FLAG_ERASER != 0 {
    mask |= StylusButtons::INVERTED;
    tool = ToolKind::Eraser;
}
```

`PEN_FLAG_INVERTED` = eraser end flipped toward screen (can fire during hover). `PEN_FLAG_ERASER` = eraser end down on screen.

## Proximity / hover

Map to `ProximitySample`:

- `WM_POINTERDEVICEINRANGE` → `is_entering: true`. Query device caps via `GetPointerDevices` / `POINTER_DEVICE_INFO` (cache per HANDLE).
- `WM_POINTERDEVICEOUTOFRANGE` → `is_entering: false`.

Within a pointer session: differentiate hover vs stroke via `POINTER_INFO.pointerFlags`:
- `POINTER_FLAG_INRANGE` (0x02): in sensing range.
- `POINTER_FLAG_INCONTACT` (0x04): tip touching.

`inrange && !incontact` → `Phase::Hover`. `inrange && incontact` → Down/Move.

## Device identification

`POINTER_INFO.sourceDevice` is a `HANDLE` stable for the device's lifetime (until unplug/driver reload). Hash into `u64` for `device_id`. Pointer API surfaces no portable per-pen serial — use `sourceDevice` hash as `unique_id` and document the limitation.

## Rust crates

`windows-sys` (narrow FFI, matches winit's own dep — one copy in tree). Features:

```toml
"Win32_Foundation",
"Win32_UI_WindowsAndMessaging",
"Win32_UI_Input_Pointer",
"Win32_UI_Shell",            # SetWindowSubclass, DefSubclassProc
"Win32_System_Performance",  # QueryPerformanceCounter/Frequency
"Win32_Graphics_Gdi",        # ScreenToClient
```

## Gotchas

- **OEM synthesized mouse events**: old Wacom/HP drivers fire `WM_MOUSEMOVE`/`WM_LBUTTONDOWN` alongside pointer messages. Swallow while `pen_in_range`.
- **History drain timing**: must be same message as the dispatch; entries lost otherwise.
- **Remote desktop / VM**: pressure often reported as constant 512 or 0. Detect via missing `PEN_MASK_PRESSURE`; fall back to 1.0 with a one-time warn.
- **Windows Ink gestures**: Win+side-button, Ink Workspace shortcut — treat `WM_POINTERCAPTURECHANGED` as Up+Cancel.
- **"Use pen as mouse" setting**: strips rich data — pen appears as `PT_MOUSE`. No programmatic opt-out; warn the user if we see a pen-class device delivering no `PT_PEN` traffic.
- **EnableMouseInPointer**: leave untouched (default FALSE). Enabling multiplexes mouse into pointer messages.

## Minimum viable implementation

1. Add `stylus-junk/src/platform/windows.rs` under `#[cfg(target_os = "windows")]`. `windows-sys` deps per above.
2. Define `WindowsStylusRawSample` / `WindowsStylusProximitySample` and adapter entry points `handle_windows_raw` / `handle_windows_proximity`.
3. Public API: `pub fn attach_to_hwnd(hwnd: isize, adapter: Rc<RefCell<StylusAdapter>>) -> WindowsStylusBackend`.
4. `SetWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID, Box::into_raw(state))`. RAII `Drop` calls `RemoveWindowSubclass`.
5. Cache `QueryPerformanceFrequency` at `attach`.
6. Subclass dispatch: branch on the seven `WM_POINTER*` messages plus the two `*DEVICE*RANGE` messages. `GetPointerType` to filter to `PT_PEN`.
7. On every update (and Down/Up for the last sample), drain `GetPointerPenInfoHistory`, iterate newest-first reversed.
8. Build samples: `ScreenToClient` for position; `pressure/1024.0`; direct tilt/rotation; tool from `penFlags`; `sourceDevice` hash for `device_id`.
9. Proximity: on `WM_POINTERDEVICE{IN,OUTOF}RANGE`, build caps bitfield from `GetPointerDevices` / `POINTER_DEVICE_INFO` (cache per device HANDLE).
10. Palm rejection: `pen_in_range` gate; swallow `PT_TOUCH` messages and legacy `WM_MOUSE*` while true.
11. Estimated/Revise: first `WM_POINTERDOWN` sample → `SampleClass::Estimated`; first follow-up with full `PEN_MASK` coverage → `Revise` through existing adapter path.
12. Tests: `stylus-junk/tests/windows_adapter.rs` driving `handle_windows_raw` with hand-built fixtures.

## Testing

- No first-party simulator. Real Surface Pen + Surface Slim Pen 2 (MPP 2.0 incl. rotation) is the primary rig. Wacom Intuos MPP pen secondary. Older N-Trig as regression canary.
- Debug logging gated on `RUST_LOG=stylus_junk::windows=debug`: on `WM_POINTERDEVICECHANGE`, enumerate and log device name + max pressure + mask bits.

## References

- [Pointer Input messages](https://learn.microsoft.com/en-us/windows/win32/inputmsg/messages-and-notifications-portal)
- [POINTER_PEN_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-pointer_pen_info)
- [GetPointerPenInfoHistory](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getpointerpeninfohistory)
- [SetWindowSubclass](https://learn.microsoft.com/en-us/windows/win32/api/commctrl/nf-commctrl-setwindowsubclass)
- [High-DPI desktop apps](https://learn.microsoft.com/en-us/windows/win32/hidpi/high-dpi-desktop-application-development-on-windows)
