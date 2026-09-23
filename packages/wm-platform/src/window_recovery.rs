//! Recovery ownership lives on HWNDs, not in the layout tree or an IPC snapshot.
//! Windows removes properties when an HWND is destroyed, preventing handle reuse.
use windows::{
  core::w,
  Win32::{
    Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM, WAIT_ABANDONED, WAIT_OBJECT_0, ERROR_INVALID_PARAMETER},
    System::Threading::{CreateMutexW, OpenProcess, ReleaseMutex, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE},
    UI::WindowsAndMessaging::{EnumWindows, GetPropW, RemovePropW, SetPropW, SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_ASYNCWINDOWPOS, WS_EX_TOPMOST},
  },
};
use crate::{NativeWindow, NativeWindowWindowsExt, OpacityValue};

const OWNER: windows::core::PCWSTR = w!("GlazeWM.Recovery.Owner.v1");
const TOPMOST: windows::core::PCWSTR = w!("GlazeWM.Recovery.Topmost.v1");

struct RecoveryLock(HANDLE);
impl RecoveryLock {
  fn acquire() -> crate::Result<Self> {
    let handle = unsafe { CreateMutexW(None, false, w!("Local\\GlazeWM.WindowRecovery.v1")) }?;
    match unsafe { WaitForSingleObject(handle, 10000) } {
      WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
      _ => {
        let _ = unsafe { CloseHandle(handle) };
        Err(crate::Error::Platform("Window recovery lock timed out".into()))
      }
    }
  }
}
impl Drop for RecoveryLock {
  fn drop(&mut self) {
    let _ = unsafe { ReleaseMutex(self.0) };
    let _ = unsafe { CloseHandle(self.0) };
  }
}

/// Register before applying any WM effects. Failure prevents managing the window.
pub fn register_window_recovery(window: &NativeWindow) -> crate::Result<()> {
  let _lock = RecoveryLock::acquire()?;
  let hwnd = window.hwnd();
  let owner = unsafe { GetPropW(hwnd, OWNER) }.0 as u32;
  if owner == std::process::id() { return Ok(()); }
  if owner != 0 {
    return Err(crate::Error::Platform("Window still belongs to another recovery session".into()));
  }
  let topmost = if window.has_window_style_ex(WS_EX_TOPMOST) { 2 } else { 1 };
  unsafe { SetPropW(hwnd, TOPMOST, HANDLE(topmost)) }?;
  unsafe { SetPropW(hwnd, OWNER, HANDLE(std::process::id() as isize)) }?;
  Ok(())
}

fn restore(window: &NativeWindow, show: bool) -> crate::Result<()> {
  // Keep the ownership record if either visibility operation fails so the
  // watcher or next launch can retry. Cosmetic failures must not block recovery.
  let uncloaked = window.set_cloaked(false);
  let shown = if show {
    if window.is_minimized().unwrap_or(false) { window.restore(None) } else { window.show() }
  } else { Ok(()) };
  let taskbar = window.set_taskbar_visibility(true);
  let _ = window.set_border_color(None);
  let _ = window.set_transparency(&OpacityValue::from_alpha(u8::MAX));
  let topmost = unsafe { GetPropW(window.hwnd(), TOPMOST) }.0 == 2;
  let _ = unsafe { SetWindowPos(window.hwnd(), if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST }, 0, 0, 0, 0,
    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_ASYNCWINDOWPOS) };
  uncloaked?;
  shown?;
  taskbar?;
  let _ = unsafe { RemovePropW(window.hwnd(), OWNER) };
  let _ = unsafe { RemovePropW(window.hwnd(), TOPMOST) };
  Ok(())
}

/// Release WM effects before forgetting a window. Preserve an application's own
/// hide/minimize-to-tray action; an explicit ignore command may request showing.
pub fn release_window_recovery(window: &NativeWindow, show: bool) -> crate::Result<()> {
  let _lock = RecoveryLock::acquire()?;
  if unsafe { GetPropW(window.hwnd(), OWNER) }.0 as u32 == std::process::id() {
    restore(window, show)?;
  }
  Ok(())
}

unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
  let windows = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
  windows.push(hwnd);
  BOOL(1)
}

fn process_exited(pid: u32) -> bool {
  match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
    Ok(handle) => {
      let exited = unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0;
      let _ = unsafe { CloseHandle(handle) };
      exited
    }
    // Access denied is not evidence of an exited process.
    Err(error) => error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0),
  }
}

/// None recovers only dead owners at startup. Some(pid) restores that session on
/// exit, including windows no longer present in the layout tree.
pub fn recover_windows(owner: Option<u32>) -> crate::Result<usize> {
  let _lock = RecoveryLock::acquire()?;
  let mut windows = Vec::<HWND>::new();
  unsafe { EnumWindows(Some(collect), LPARAM((&raw mut windows) as isize)) }?;
  let mut restored = 0;
  for hwnd in windows {
    let pid = unsafe { GetPropW(hwnd, OWNER) }.0 as u32;
    if pid == 0 || !owner.map_or_else(|| process_exited(pid), |expected| pid == expected) { continue; }
    let window = NativeWindow::from_handle(hwnd.0);
    match restore(&window, true) {
      Ok(()) => restored += 1,
      Err(error) => tracing::warn!("Window recovery failed for {}: {error}", hwnd.0),
    }
  }
  Ok(restored)
}

/// Blocking OS wait: no polling and no dependency on the manager's IPC lifetime.
pub fn wait_for_manager_exit(pid: u32, ready: impl FnOnce()) -> crate::Result<()> {
  match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
    Ok(handle) => {
      ready();
      let result = unsafe { WaitForSingleObject(handle, INFINITE) };
      let _ = unsafe { CloseHandle(handle) };
      if result != WAIT_OBJECT_0 { return Err(crate::Error::Platform("Manager exit wait failed".into())); }
      Ok(())
    }
    Err(error) if error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => Ok(()),
    Err(error) => Err(error.into()),
  }
}
