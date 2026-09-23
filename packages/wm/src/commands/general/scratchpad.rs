use anyhow::Context;
use uuid::Uuid;
use wm_common::{FloatingStateConfig, ScratchpadDto, WindowState, WmEvent};
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

pub fn scratchpad_add(window: WindowContainer, state: &mut WmState, config: &UserConfig) -> anyhow::Result<()> {
  if state.scratchpad.as_ref().is_some_and(|s| s.id == window.id()) { return Ok(()); }
  scratchpad_remove(state, config)?;
  let record = Scratchpad { id: window.id(), original_state: window.state(), original_placement: window.floating_placement() };
  let window = update_window_state(window, floating(), state, config)?;
  let area = window.workspace().context("No workspace")?.to_rect()?;
  window.set_floating_placement(Rect::from_xy(0, 0, 900.min(area.width()), 600.min(area.height())).translate_to_center(&area));
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
  let window = tracked(state).context("No scratchpad window; mark a window first")?;
  if window.state() != WindowState::Minimized {
    update_window_state(window, WindowState::Minimized, state, config)?;
  } else {
    let workspace = state.focused_container().and_then(|c| c.workspace()).context("No focused workspace")?;
    move_window_to_workspace(window.clone(), WorkspaceTarget::Name(workspace.config().name), state, config)?;
    let window = update_window_state(window, floating(), state, config)?;
    focus_container_by_id(&window.id(), state)?;
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
