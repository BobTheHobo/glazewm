//! Window-recovery health for diagnostics (`query recovery`) and the manual
//! `recover-untracked-windows` action. Records live on HWNDs; this module only
//! compares them with the layout tree, including saved workspaces.
use std::collections::HashSet;

use wm_common::{RecoveryData, RecoveryEventDto, RecoveryRecordDto, RecoveryWatcherDto};

use crate::{traits::WindowGetters, wm_state::WmState};

fn managed_handles(state: &WmState) -> HashSet<isize> {
  state.windows().into_iter().map(|window| window.native().id().0).collect()
}

#[cfg(target_os = "windows")]
pub fn recovery_status(state: &WmState) -> anyhow::Result<RecoveryData> {
  use wm_platform::RecordStatus;

  let managed = managed_handles(state);
  let records = wm_platform::recovery_records(|hwnd| managed.contains(&hwnd))?;
  let managed_record_count = records.iter().filter(|r| r.status == RecordStatus::Managed).count();
  let unmanaged_records = records
    .into_iter()
    .filter(|r| r.status != RecordStatus::Managed)
    .map(|r| RecoveryRecordDto {
      handle: r.hwnd,
      process_name: r.process_name,
      status: match r.status {
        RecordStatus::Managed => "managed",
        RecordStatus::Untracked => "untracked",
        RecordStatus::Orphaned => "orphaned",
        RecordStatus::OtherSession => "otherSession",
      }
      .to_string(),
      owner_pid: r.owner_pid,
      visible: r.visible,
      minimized: r.minimized,
      cloak: r.cloak,
    })
    .collect();
  let (pid, running) = wm_platform::recovery_watcher_status().map_or((None, false), |(pid, running)| (Some(pid), running));

  Ok(RecoveryData {
    recovery_watcher: RecoveryWatcherDto { pid, running },
    unmanaged_records,
    managed_record_count,
    recovery_events: wm_platform::recovery_events()
      .into_iter()
      .map(|e| RecoveryEventDto {
        unix_ms: e.unix_ms,
        handle: e.hwnd,
        process_name: e.process_name,
        reason: e.reason.to_string(),
        error: e.error,
      })
      .collect(),
  })
}

#[cfg(not(target_os = "windows"))]
pub fn recovery_status(_state: &WmState) -> anyhow::Result<RecoveryData> {
  Ok(RecoveryData {
    recovery_watcher: RecoveryWatcherDto { pid: None, running: false },
    unmanaged_records: Vec::new(),
    managed_record_count: 0,
    recovery_events: Vec::new(),
  })
}

/// Uncloaks untracked and orphaned windows; GlazeWM manages them again when
/// Windows reports them shown. Fails if any restore fails, after trying all.
#[cfg(target_os = "windows")]
pub fn recover_untracked_windows(state: &mut WmState) -> anyhow::Result<()> {
  let managed = managed_handles(state);
  let (restored, failed) = wm_platform::recover_untracked_windows(|hwnd| managed.contains(&hwnd))?;
  tracing::info!("Manual window recovery restored {restored}, failed {failed}");
  anyhow::ensure!(failed == 0, "{failed} window(s) could not be restored; see `query recovery`.");
  Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn recover_untracked_windows(_state: &mut WmState) -> anyhow::Result<()> {
  Ok(())
}
