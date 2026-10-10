//! Recovery ownership lives on HWNDs, not in the layout tree or an IPC snapshot.
//! Windows removes properties when an HWND is destroyed, preventing handle reuse.
//!
//! An owner is a manager process identified by PID *and* creation time, so a
//! PID that Windows has since reused for an unrelated process is never mistaken
//! for a live manager. Records written before the creation time existed (no
//! OWNER_START) fall back to the conservative PID-only check.
use std::{
  collections::VecDeque,
  sync::{Mutex, OnceLock},
  time::{SystemTime, UNIX_EPOCH},
};

use windows::{
  core::w,
  Win32::{
    Foundation::{CloseHandle, BOOL, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, HWND, LPARAM, STILL_ACTIVE, WAIT_ABANDONED, WAIT_OBJECT_0},
    Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
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
  // Skip the uncloak when DWM already reports no cloak: windows without an
  // application view (e.g. non-taskbar tool windows) make it fail with
  // "Element not found", which would otherwise leave a stale record behind.
  let uncloaked = if cloak_bits(window.hwnd()) == Some(0) { Ok(()) } else { window.set_cloaked(false) };
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
    let result = restore(window, show);
    // A window destroyed while it is being released has nothing left to
    // recover (its properties went with it); that isn't a recovery failure.
    let result = if release_failure_is_real(result.is_err(), window.is_valid()) { result } else { Ok(()) };
    // Releases are routine; only failures belong in the diagnostics history.
    if result.is_err() { note_recovery(window, "release", &result); }
    result?;
  }
  Ok(())
}

/// A failed release matters only while the window still exists.
fn release_failure_is_real(failed: bool, window_still_valid: bool) -> bool {
  failed && window_still_valid
}

/// How a window's recovery record relates to the current manager session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordStatus {
  /// Ours and in the layout tree.
  Managed,
  /// Ours but no longer in the layout tree: GlazeWM lost track of it.
  Untracked,
  /// Written by a manager session that is no longer running.
  Orphaned,
  /// Owned by another manager session that is still running.
  OtherSession,
}

/// Pure classification of one record, kept separate for tests.
pub fn classify_record(owner_pid: u32, owner_started: Option<u64>, me: RecoveryOwner, owner_running: bool, managed: bool) -> RecordStatus {
  if me.matches(Record { pid: owner_pid, started: owner_started }) {
    if managed { RecordStatus::Managed } else { RecordStatus::Untracked }
  } else if owner_pid == me.pid || !owner_running {
    // Our PID with another start time is a dead earlier session.
    RecordStatus::Orphaned
  } else {
    RecordStatus::OtherSession
  }
}

/// One window carrying a recovery record, with the native state that decides
/// whether it is actually visible.
#[derive(Clone, Debug)]
pub struct RecoveryRecordInfo {
  pub hwnd: isize,
  pub owner_pid: u32,
  pub owner_started: Option<u64>,
  pub status: RecordStatus,
  pub process_name: Option<String>,
  pub visible: bool,
  pub minimized: bool,
  /// DWM cloak bits (1 app, 2 shell, 4 inherited); None if the query failed.
  pub cloak: Option<u32>,
}

fn cloak_bits(hwnd: HWND) -> Option<u32> {
  let mut value = 0u32;
  #[allow(clippy::cast_possible_truncation)]
  unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, std::ptr::from_mut::<u32>(&mut value).cast(), std::mem::size_of::<u32>() as u32) }.ok()?;
  Some(value)
}

fn all_windows() -> crate::Result<Vec<HWND>> {
  let mut windows = Vec::<HWND>::new();
  unsafe { EnumWindows(Some(collect), LPARAM((&raw mut windows) as isize)) }?;
  Ok(windows)
}

fn classify_hwnd(hwnd: HWND, record: Record, me: RecoveryOwner, is_managed: &impl Fn(isize) -> bool) -> RecordStatus {
  let ours = me.matches(record);
  classify_record(record.pid, record.started, me, ours || owner_alive(record), is_managed(hwnd.0))
}

/// Every top-level window carrying a recovery record. Read-only.
pub fn recovery_records(is_managed: impl Fn(isize) -> bool) -> crate::Result<Vec<RecoveryRecordInfo>> {
  let _lock = RecoveryLock::acquire()?;
  let me = RecoveryOwner::current();
  let mut records = Vec::new();
  for hwnd in all_windows()? {
    let Some(record) = record_of(hwnd) else { continue };
    let window = NativeWindow::from_handle(hwnd.0);
    records.push(RecoveryRecordInfo {
      hwnd: hwnd.0,
      owner_pid: record.pid,
      owner_started: record.started,
      status: classify_hwnd(hwnd, record, me, &is_managed),
      process_name: window.process_name().ok(),
      visible: window.is_visible().unwrap_or(false),
      minimized: window.is_minimized().unwrap_or(false),
      cloak: cloak_bits(hwnd),
    });
  }
  Ok(records)
}

/// Restores windows GlazeWM owns but no longer manages (untracked or from a
/// dead session). Only windows carrying a recovery record are touched, nothing
/// is closed, and application-hidden windows are not shown: the window is
/// uncloaked and returned to the taskbar, which lets GlazeWM manage it again.
pub fn recover_untracked_windows(is_managed: impl Fn(isize) -> bool) -> crate::Result<(usize, usize)> {
  let _lock = RecoveryLock::acquire()?;
  let me = RecoveryOwner::current();
  let (mut restored, mut failed) = (0, 0);
  for hwnd in all_windows()? {
    let Some(record) = record_of(hwnd) else { continue };
    if !matches!(classify_hwnd(hwnd, record, me, &is_managed), RecordStatus::Untracked | RecordStatus::Orphaned) { continue; }
    let window = NativeWindow::from_handle(hwnd.0);
    let result = restore(&window, false);
    note_recovery(&window, "manual", &result);
    if result.is_ok() { restored += 1 } else { failed += 1 }
  }
  Ok((restored, failed))
}

/// A recovery attempt, kept in a small in-memory history for diagnostics.
#[derive(Clone, Debug)]
pub struct RecoveryEvent {
  pub unix_ms: u64,
  pub hwnd: isize,
  pub process_name: Option<String>,
  /// "startup", "exit", "manual" or "release".
  pub reason: &'static str,
  pub error: Option<String>,
}

const RECOVERY_EVENT_LIMIT: usize = 25;

fn recovery_event_log() -> &'static Mutex<VecDeque<RecoveryEvent>> {
  static EVENTS: OnceLock<Mutex<VecDeque<RecoveryEvent>>> = OnceLock::new();
  EVENTS.get_or_init(|| Mutex::new(VecDeque::with_capacity(RECOVERY_EVENT_LIMIT)))
}

fn note_recovery(window: &NativeWindow, reason: &'static str, result: &crate::Result<()>) {
  let event = RecoveryEvent {
    unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
    hwnd: window.hwnd().0,
    process_name: window.process_name().ok(),
    reason,
    error: result.as_ref().err().map(ToString::to_string),
  };
  if let Ok(mut events) = recovery_event_log().lock() {
    if events.len() == RECOVERY_EVENT_LIMIT { events.pop_front(); }
    events.push_back(event);
  }
}

/// Most recent recovery attempts in this manager session, oldest first.
pub fn recovery_events() -> Vec<RecoveryEvent> {
  recovery_event_log().lock().map(|events| events.iter().cloned().collect()).unwrap_or_default()
}

fn watcher_slot() -> &'static Mutex<Option<RecoveryOwner>> {
  static WATCHER: OnceLock<Mutex<Option<RecoveryOwner>>> = OnceLock::new();
  WATCHER.get_or_init(|| Mutex::new(None))
}

/// Remember the watcher launched for this manager, by PID and start time.
pub fn set_recovery_watcher(pid: u32) {
  let started = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok().and_then(|handle| {
    let started = process_start(handle);
    let _ = unsafe { CloseHandle(handle) };
    started
  });
  if let (Some(started), Ok(mut slot)) = (started, watcher_slot().lock()) {
    *slot = Some(RecoveryOwner { pid, started });
  }
}

/// The recorded watcher PID and whether that exact process is still running.
pub fn recovery_watcher_status() -> Option<(u32, bool)> {
  let watcher = (*watcher_slot().lock().ok()?)?;
  Some((watcher.pid, owner_alive(Record { pid: watcher.pid, started: Some(watcher.started) })))
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
  let me = RecoveryOwner::current();
  let reason = if owner.is_some() { "exit" } else { "startup" };
  let mut restored = 0;
  for hwnd in all_windows()? {
    let Some(record) = record_of(hwnd) else { continue };
    let orphaned = match owner {
      Some(expected) => expected.matches(record),
      None => !(record.pid == me.pid && record.started == Some(me.started)) && !owner_alive(record),
    };
    if !orphaned { continue; }
    let window = NativeWindow::from_handle(hwnd.0);
    let result = restore(&window, true);
    note_recovery(&window, reason, &result);
    match result {
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
  fn release_failure_on_a_destroyed_window_is_not_a_failure() {
    assert!(release_failure_is_real(true, true));
    assert!(!release_failure_is_real(true, false));
    assert!(!release_failure_is_real(false, true));
    assert!(!release_failure_is_real(false, false));
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
  fn records_are_classified_against_the_current_session() {
    let me = RecoveryOwner { pid: 10, started: 100 };
    assert_eq!(classify_record(10, Some(100), me, true, true), RecordStatus::Managed);
    // Ours but missing from the layout tree: GlazeWM lost track of it.
    assert_eq!(classify_record(10, Some(100), me, true, false), RecordStatus::Untracked);
    // A legacy record with our PID counts as ours.
    assert_eq!(classify_record(10, None, me, true, false), RecordStatus::Untracked);
    // Our PID but an earlier start time is a dead session that reused the PID.
    assert_eq!(classify_record(10, Some(99), me, true, false), RecordStatus::Orphaned);
    assert_eq!(classify_record(20, Some(5), me, false, false), RecordStatus::Orphaned);
    assert_eq!(classify_record(20, Some(5), me, true, false), RecordStatus::OtherSession);
  }

  #[test]
  fn recovery_history_is_bounded_and_keeps_the_newest() {
    let window = NativeWindow::from_handle(0);
    for _ in 0..(RECOVERY_EVENT_LIMIT + 5) { note_recovery(&window, "manual", &Ok(())); }
    note_recovery(&window, "startup", &Err(crate::Error::Platform("boom".into())));
    let events = recovery_events();
    assert_eq!(events.len(), RECOVERY_EVENT_LIMIT);
    assert_eq!(events.last().unwrap().reason, "startup");
    assert!(events.last().unwrap().error.as_deref().is_some_and(|e| e.contains("boom")));
  }

  #[test]
  fn watcher_status_tracks_the_exact_process() {
    let mut process = child();
    set_recovery_watcher(process.id());
    assert_eq!(recovery_watcher_status(), Some((process.id(), true)));
    process.kill().unwrap();
    process.wait().unwrap();
    assert_eq!(recovery_watcher_status(), Some((process.id(), false)));
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
