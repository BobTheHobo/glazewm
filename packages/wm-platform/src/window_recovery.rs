//! Recovery ownership lives on HWNDs, not in the layout tree or an IPC snapshot.
//! Windows removes properties when an HWND is destroyed, preventing handle reuse.
//!
//! An owner is a manager process identified by PID *and* creation time, so a
//! PID that Windows has since reused for an unrelated process is never mistaken
//! for a live manager. Records written before the creation time existed (no
//! OWNER_START) fall back to the conservative PID-only check.
use std::sync::OnceLock;

use windows::{
  core::w,
  Win32::{
    Foundation::{CloseHandle, BOOL, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, HWND, LPARAM, STILL_ACTIVE, WAIT_ABANDONED, WAIT_OBJECT_0},
    System::Threading::{CreateMutexW, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess, ReleaseMutex, WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE},
    UI::WindowsAndMessaging::{EnumWindows, GetPropW, RemovePropW, SetPropW, SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_ASYNCWINDOWPOS, WS_EX_TOPMOST},
  },
};
use crate::{NativeWindow, NativeWindowWindowsExt, OpacityValue};

const OWNER: windows::core::PCWSTR = w!("GlazeWM.Recovery.Owner.v1");
const OWNER_START: windows::core::PCWSTR = w!("GlazeWM.Recovery.OwnerStart.v1");
const TOPMOST: windows::core::PCWSTR = w!("GlazeWM.Recovery.Topmost.v1");

/// A manager session: PID plus process creation time (FILETIME ticks).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryOwner {
  pub pid: u32,
  pub started: u64,
}

impl RecoveryOwner {
  /// The current process's session identity.
  pub fn current() -> Self {
    static CURRENT: OnceLock<RecoveryOwner> = OnceLock::new();
    *CURRENT.get_or_init(|| RecoveryOwner {
      pid: std::process::id(),
      started: process_start(unsafe { GetCurrentProcess() }).unwrap_or(0),
    })
  }

  fn matches(&self, record: Record) -> bool {
    record.pid == self.pid && record.started.map_or(true, |started| started == self.started)
  }
}

/// What a window's properties say about its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record {
  pid: u32,
  /// None for records written by builds before 3.10.1-focus.5.
  started: Option<u64>,
}

fn record_of(hwnd: HWND) -> Option<Record> {
  let pid = unsafe { GetPropW(hwnd, OWNER) }.0 as u32;
  if pid == 0 { return None; }
  let started = unsafe { GetPropW(hwnd, OWNER_START) }.0 as u64;
  Some(Record { pid, started: (started != 0).then_some(started) })
}

fn write_owner(hwnd: HWND, owner: RecoveryOwner) -> crate::Result<()> {
  // OWNER last: its presence is what marks a complete record.
  unsafe { SetPropW(hwnd, OWNER_START, HANDLE(owner.started as isize)) }?;
  unsafe { SetPropW(hwnd, OWNER, HANDLE(owner.pid as isize)) }?;
  Ok(())
}

fn process_start(process: HANDLE) -> Option<u64> {
  let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
  unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
  Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// Whether the manager that wrote `record` is still running.
fn owner_alive(record: Record) -> bool {
  let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, record.pid) } {
    Ok(handle) => handle,
    Err(error) if error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => return false,
    // A manager runs as this user and can always be queried by another
    // instance, so an inaccessible process holding a recorded PID is some
    // other (reused) process. Legacy PID-only records stay conservative.
    Err(_) => return record.started.is_none(),
  };
  let started = process_start(handle);
  let mut code = 0u32;
  let running = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok() && code == STILL_ACTIVE.0 as u32;
  let _ = unsafe { CloseHandle(handle) };
  running && record.started.map_or(true, |expected| started == Some(expected))
}

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
  let me = RecoveryOwner::current();
  match record_of(hwnd) {
    Some(record) if record.pid == me.pid && record.started == Some(me.started) => return Ok(()),
    // Our PID but another session's record is a dead manager whose PID we
    // reused; any other live owner is a different running manager.
    Some(record) if record.pid != me.pid && owner_alive(record) => {
      return Err(crate::Error::Platform("Window still belongs to another recovery session".into()));
    }
    // Dead owner: take over, keeping the original topmost state it recorded.
    Some(_) => {}
    None => {
      let topmost = if window.has_window_style_ex(WS_EX_TOPMOST) { 2 } else { 1 };
      unsafe { SetPropW(hwnd, TOPMOST, HANDLE(topmost)) }?;
    }
  }
  write_owner(hwnd, me)
}

fn restore(window: &NativeWindow, show: bool) -> crate::Result<()> {
  // Keep the ownership record if either visibility operation fails so the
  // watcher or next launch can retry. Cosmetic failures must not block recovery.
  crate::trace_window_tracking(window, "recovery-before");
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
  crate::trace_window_tracking(window, "recovery-after");
  tracing::info!(target: "window_tracking", hwnd = window.hwnd().0, show, uncloak_ok = uncloaked.is_ok(), show_ok = shown.is_ok(), taskbar_ok = taskbar.is_ok(), "recovery-result");
  uncloaked?;
  shown?;
  taskbar?;
  let _ = unsafe { RemovePropW(window.hwnd(), OWNER) };
  let _ = unsafe { RemovePropW(window.hwnd(), OWNER_START) };
  let _ = unsafe { RemovePropW(window.hwnd(), TOPMOST) };
  Ok(())
}

/// Release WM effects before forgetting a window. Preserve an application's own
/// hide/minimize-to-tray action; an explicit ignore command may request showing.
pub fn release_window_recovery(window: &NativeWindow, show: bool) -> crate::Result<()> {
  let _lock = RecoveryLock::acquire()?;
  if record_of(window.hwnd()).is_some_and(|record| RecoveryOwner::current().matches(record)) {
    restore(window, show)?;
  }
  Ok(())
}

unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
  let windows = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
  windows.push(hwnd);
  BOOL(1)
}

/// None recovers records from dead owners at startup (never this session's).
/// Some(owner) restores that session on exit, including windows no longer
/// present in the layout tree.
pub fn recover_windows(owner: Option<RecoveryOwner>) -> crate::Result<usize> {
  let _lock = RecoveryLock::acquire()?;
  let mut windows = Vec::<HWND>::new();
  unsafe { EnumWindows(Some(collect), LPARAM((&raw mut windows) as isize)) }?;
  let me = RecoveryOwner::current();
  let mut restored = 0;
  for hwnd in windows {
    let Some(record) = record_of(hwnd) else { continue };
    let orphaned = match owner {
      Some(expected) => expected.matches(record),
      None => !(record.pid == me.pid && record.started == Some(me.started)) && !owner_alive(record),
    };
    if !orphaned { continue; }
    let window = NativeWindow::from_handle(hwnd.0);
    match restore(&window, true) {
      Ok(()) => restored += 1,
      Err(error) => tracing::warn!("Window recovery failed for {}: {error}", hwnd.0),
    }
  }
  Ok(restored)
}

/// Blocking OS wait, then recovery of that manager's windows. No polling and no
/// dependency on the manager's IPC lifetime. The process handle stays open
/// until recovery finishes, so Windows cannot reuse the PID meanwhile.
pub fn watch_manager(pid: u32, ready: impl FnOnce()) -> crate::Result<usize> {
  let handle = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
    Ok(handle) => handle,
    // The manager waits for "ready" before managing any window, so a manager
    // that is already gone registered nothing.
    Err(error) if error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) => return Ok(0),
    Err(error) => return Err(error.into()),
  };
  let result = (|| {
    let started = process_start(handle).ok_or_else(|| crate::Error::Platform("Cannot read manager start time".into()))?;
    ready();
    if unsafe { WaitForSingleObject(handle, INFINITE) } != WAIT_OBJECT_0 {
      return Err(crate::Error::Platform("Manager exit wait failed".into()));
    }
    recover_windows(Some(RecoveryOwner { pid, started }))
  })();
  let _ = unsafe { CloseHandle(handle) };
  result
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::windows::io::AsRawHandle;

  fn child() -> std::process::Child {
    std::process::Command::new("cmd").args(["/c", "ping", "-n", "30", "127.0.0.1"])
      .stdout(std::process::Stdio::null()).spawn().unwrap()
  }

  fn identity(child: &std::process::Child) -> RecoveryOwner {
    RecoveryOwner { pid: child.id(), started: process_start(HANDLE(child.as_raw_handle() as isize)).unwrap() }
  }

  #[test]
  fn current_session_has_a_start_time() {
    assert_ne!(RecoveryOwner::current().started, 0);
    assert!(owner_alive(Record { pid: std::process::id(), started: Some(RecoveryOwner::current().started) }));
  }

  #[test]
  fn live_owner_with_matching_start_is_alive() {
    let mut process = child();
    let owner = identity(&process);
    assert!(owner_alive(Record { pid: owner.pid, started: Some(owner.started) }));
    process.kill().unwrap();
    process.wait().unwrap();
  }

  #[test]
  fn reused_pid_with_different_start_is_dead() {
    // Simulates Windows handing a dead manager's PID to an unrelated process.
    let mut process = child();
    let owner = identity(&process);
    assert!(!owner_alive(Record { pid: owner.pid, started: Some(owner.started - 1) }));
    // A legacy PID-only record stays conservative: a live PID counts as alive.
    assert!(owner_alive(Record { pid: owner.pid, started: None }));
    process.kill().unwrap();
    process.wait().unwrap();
  }

  #[test]
  fn exited_owner_is_dead_even_while_its_pid_is_held() {
    let mut process = child();
    let owner = identity(&process);
    process.kill().unwrap();
    process.wait().unwrap();
    // `process` still holds its handle, so the PID is not yet reusable.
    assert!(!owner_alive(Record { pid: owner.pid, started: Some(owner.started) }));
  }

  #[test]
  fn session_matching_accepts_legacy_records_only_for_the_same_pid() {
    let me = RecoveryOwner { pid: 42, started: 7 };
    assert!(me.matches(Record { pid: 42, started: Some(7) }));
    assert!(me.matches(Record { pid: 42, started: None }));
    assert!(!me.matches(Record { pid: 42, started: Some(8) }));
    assert!(!me.matches(Record { pid: 43, started: Some(7) }));
  }
}
