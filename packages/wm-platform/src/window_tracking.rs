//! Read-only native snapshots. Never query titles, document paths or COM here.
use windows::{core::w, Win32::{Foundation::{HWND, BOOL, LPARAM}, Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
  UI::WindowsAndMessaging::{EnumWindows, GetWindowLongPtrW, GWL_STYLE, WS_CAPTION, WS_CHILD, GetPropW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, IsIconic}}};
use crate::{NativeWindow, NativeWindowWindowsExt};

pub fn trace_window_tracking(window: &NativeWindow, phase: &'static str) {
  trace_tracking_handle(window.hwnd().0, phase, 0, 0);
}

pub fn trace_tracking_handle(handle: isize, phase: &'static str, raw_event: u32, event_time: u32) {
  let hwnd = HWND(handle);
  let mut pid = 0;
  let mut cloak = 0u32;
  unsafe {
    let owner = GetPropW(hwnd, w!("GlazeWM.Recovery.Owner.v1")).0;
    // Unowned destroyed helper windows are high-volume and have no WM lifecycle.
    if raw_event == 0x8001 && owner == 0 { return; }
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
    // Keep owned windows and normal top-level application windows, not controls.
    if owner == 0 && IsWindow(hwnd).as_bool()
      && (style & WS_CHILD.0 != 0 || style & WS_CAPTION.0 == 0) { return; }
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let result = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloak).cast(), 4);
    tracing::info!(target: "window_tracking", phase, hwnd = handle, pid, raw_event, event_time,
      valid = IsWindow(hwnd).as_bool(), visible = IsWindowVisible(hwnd).as_bool(),
      minimized = IsIconic(hwnd).as_bool(), cloak = ?result.as_ref().ok().map(|_| cloak),
      cloak_error = ?result.err().map(|e| e.code().0),
      recovery_owner = owner,
      recovery_start = GetPropW(hwnd, w!("GlazeWM.Recovery.OwnerStart.v1")).0,
      "native-state");
  }
}

pub fn trace_tracking_inventory(phase: &'static str) {
  unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
    let handles = unsafe { &mut *(data.0 as *mut Vec<isize>) };
    handles.push(hwnd.0);
    BOOL(1)
  }
  let mut handles = Vec::<isize>::new();
  let result = unsafe { EnumWindows(Some(collect), LPARAM((&raw mut handles) as isize)) };
  tracing::info!(target: "window_tracking", phase, count = handles.len(), ok = result.is_ok(), "inventory");
  for hwnd in handles { trace_tracking_handle(hwnd, phase, 0, 0); }
}
