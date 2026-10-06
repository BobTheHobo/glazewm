use anyhow::Context;
use uuid::Uuid;
use wm_common::{FloatingStateConfig, ScratchpadDto, ScratchpadRestoreState, WindowState, WmEvent};
use wm_platform::Rect;
use crate::{
  commands::{container::focus_container_by_id, window::{move_window_to_workspace, update_window_state}},
  models::{WindowContainer, WorkspaceTarget},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig, wm_state::WmState,
};

// Session-local ownership uses a managed UUID, never a reusable native handle.
pub struct Scratchpad {
  pub id: Uuid,
  original_state: WindowState,
  original_placement: Rect,
}

fn tracked(state: &WmState) -> Option<WindowContainer> {
  state.container_by_id(state.scratchpad.as_ref()?.id)?.as_window_container().ok()
}

fn floating() -> WindowState {
  WindowState::Floating(FloatingStateConfig { centered: false, shown_on_top: true })
}

pub fn scratchpad_add(window: WindowContainer, restore_json: Option<&str>, state: &mut WmState, config: &UserConfig) -> anyhow::Result<()> {
  let restore = restore_json.map(parse_restore_state).transpose()?;
  if restore.is_none() && state.scratchpad.as_ref().is_some_and(|s| s.id == window.id()) { return Ok(()); }
  // Releasing the same window may replace its container; reacquire by UUID.
  let id = window.id();
  scratchpad_remove(state, config)?;
  let window = state.container_by_id(id).context("Scratchpad window disappeared")?.as_window_container()?;
  let record = Scratchpad { id, original_state: restore.as_ref().map_or_else(|| window.state(), |r| r.original_state.clone()),
    original_placement: restore.as_ref().map_or_else(|| window.floating_placement(), |r| r.original_placement.clone()) };
  let window = update_window_state(window, floating(), state, config)?;
  let area = window.workspace().context("No workspace")?.to_rect()?;
  window.set_floating_placement(restore.as_ref().map_or_else(
    || Rect::from_xy(0, 0, 900.min(area.width()), 600.min(area.height())).translate_to_center(&area),
    |r| r.overlay_placement.clone()));
  let window = if restore.as_ref().is_some_and(|r| r.hidden) {
    update_window_state(window, WindowState::Minimized, state, config)?
  } else { window };
  state.scratchpad = Some(record);
  state.pending_sync.queue_container_to_redraw(window).queue_all_effects_update();
  Ok(())
}

pub fn scratchpad_remove(state: &mut WmState, config: &UserConfig) -> anyhow::Result<()> {
  let window = tracked(state);
  if let Some(record) = state.scratchpad.take() {
    if let Some(window) = window {
      window.set_floating_placement(record.original_placement);
      let original = if record.original_state == WindowState::Minimized { WindowState::Tiling } else { record.original_state };
      let window = update_window_state(window, original, state, config)?;
      state.pending_sync.queue_container_to_redraw(window).queue_all_effects_update();
    }
  }
  Ok(())
}

pub fn scratchpad_toggle(state: &mut WmState, config: &UserConfig) -> anyhow::Result<()> {
  let Some(window) = tracked(state) else { return scratchpad_hint(state); };
  if window.state() != WindowState::Minimized {
    update_window_state(window, WindowState::Minimized, state, config)?;
  } else {
    let workspace = state.focused_container().and_then(|c| c.workspace()).context("No focused workspace")?;
    move_window_to_workspace(window.clone(), WorkspaceTarget::Name(workspace.config().name), state, config)?;
    let window = update_window_state(window, floating(), state, config)?;
    focus_container_by_id(&window.id(), state, config)?;
  }
  Ok(())
}

// Called in the manager's existing event loop: no polling or subprocesses.
pub fn sync_scratchpad(state: &mut WmState, config: &UserConfig) -> anyhow::Result<()> {
  let window = tracked(state);
  if window.is_none() { state.scratchpad = None; }
  if let Some(ref window) = window {
    if window.state() != WindowState::Minimized {
      if let Some(workspace) = state.focused_container().and_then(|c| c.workspace()) {
        if window.workspace().is_some_and(|w| w.id() != workspace.id()) {
          move_window_to_workspace(window.clone(), WorkspaceTarget::Name(workspace.config().name), state, config)?;
        }
      }
    }
  }
  let status = window.map(|w| ScratchpadDto {
    id: w.id(), hidden: w.state() == WindowState::Minimized,
    title: w.native_properties().title.clone(), process_name: w.native_properties().process_name.clone(),
    restore_state: state.scratchpad.as_ref().map(|record| ScratchpadRestoreState {
      original_state: record.original_state.clone(), original_placement: record.original_placement.clone(),
      overlay_placement: w.floating_placement(), hidden: w.state() == WindowState::Minimized,
    }),
  });
  for workspace in state.workspaces() {
    if workspace.scratchpad() != status {
      workspace.set_scratchpad(status.clone());
      state.emit_event(WmEvent::WorkspaceUpdated { updated_workspace: workspace.to_dto()? });
    }
  }
  Ok(())
}

#[cfg(test)]
mod scratchpad_state_tests {
  use super::parse_restore_state;
  fn snapshot() -> serde_json::Value {
    serde_json::json!({
      "originalState": {"type":"floating", "centered":false, "shownOnTop":false},
      "originalPlacement": {"left":100,"top":90,"right":700,"bottom":490},
      "overlayPlacement": {"left":320,"top":210,"right":1140,"bottom":730},
      "hidden":true
    })
  }
  #[test]
  fn restore_preserves_separate_release_and_overlay_state() {
    let restored = parse_restore_state(&snapshot().to_string()).unwrap();
    assert!(restored.hidden);
    assert_eq!(restored.original_placement.left, 100);
    assert_eq!(restored.overlay_placement.left, 320);
    let json = serde_json::to_string(&restored).unwrap();
    assert_eq!(parse_restore_state(&json).unwrap(), restored);
  }
  #[test]
  fn restore_rejects_incomplete_or_invalid_rectangles() {
    assert!(parse_restore_state("{}").is_err());
    for value in [100, i32::MIN] {
      let mut data = snapshot();
      data["originalPlacement"]["right"] = value.into();
      assert!(parse_restore_state(&data.to_string()).is_err());
    }
    let mut data = snapshot();
    data["overlayPlacement"]["left"] = i32::MIN.into();
    data["overlayPlacement"]["right"] = i32::MAX.into();
    assert!(parse_restore_state(&data.to_string()).is_err());
  }
  use wm_common::{FloatingStateConfig, FullscreenStateConfig};
  #[test]
  fn topmost_survives_ipc_and_yaml_spelling() {
    let original = FloatingStateConfig { centered: false, shown_on_top: true };
    let json = serde_json::to_string(&original).unwrap();
    let roundtrip: FloatingStateConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtrip, original);
    let yaml_spelling: FloatingStateConfig = serde_json::from_str(r#"{"shown_on_top":true}"#).unwrap();
    assert!(yaml_spelling.shown_on_top);
    let fullscreen: FullscreenStateConfig = serde_json::from_str(r#"{"shownOnTop":true}"#).unwrap();
    assert!(fullscreen.shown_on_top);
  }
}

pub fn scratchpad_hint(state: &mut WmState) -> anyhow::Result<()> {
  if let Some(workspace) = state.focused_container().and_then(|c| c.workspace()) {
    workspace.show_scratchpad_hint();
    state.emit_event(WmEvent::WorkspaceUpdated { updated_workspace: workspace.to_dto()? });
  }
  Ok(())
}

fn parse_restore_state(json: &str) -> anyhow::Result<ScratchpadRestoreState> {
  let value: ScratchpadRestoreState = serde_json::from_str(json).context("Invalid scratchpad restore snapshot")?;
  for rect in [&value.original_placement, &value.overlay_placement] {
    anyhow::ensure!(rect.right.checked_sub(rect.left).is_some_and(|n| n > 0)
      && rect.bottom.checked_sub(rect.top).is_some_and(|n| n > 0), "Invalid scratchpad restore rectangle");
  }
  Ok(value)
}
