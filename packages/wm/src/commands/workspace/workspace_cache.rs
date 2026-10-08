use anyhow::{Context, ensure};
use wm_common::{SavedWorkspaceDto, WorkspaceConfig, WmEvent};
use crate::{
  commands::{container::{attach_container, set_focused_descendant}, general::scratchpad_remove},
  models::Workspace,
  traits::{CommonGetters, PositionGetters, TilingDirectionGetters, WindowGetters},
  user_config::UserConfig, wm_state::WmState,
};
use super::{activate_workspace, deactivate_workspace};

fn metadata(name: String) -> SavedWorkspaceDto {
  SavedWorkspaceDto { original_name: name, saved_at: std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64 }
}

fn release_scratchpad(workspace: &Workspace, state: &mut WmState, config: &UserConfig) -> anyhow::Result<()> {
  if state.scratchpad.as_ref().is_some_and(|s| workspace.descendants().any(|c| c.id() == s.id)) {
    scratchpad_remove(state, config)?;
  }
  Ok(())
}

fn changed(workspace: &Workspace, state: &mut WmState) -> anyhow::Result<()> {
  state.pending_sync.queue_container_to_redraw(workspace.clone());
  state.emit_event(WmEvent::WorkspaceUpdated { updated_workspace: workspace.to_dto()? });
  Ok(())
}

pub fn save_workspace(workspace: &Workspace, state: &mut WmState, config: &UserConfig) -> anyhow::Result<()> {
  ensure!(!workspace.is_saved(), "This workspace is already saved.");
  ensure!(workspace.has_children(), "This workspace has no windows to save.");
  let monitor = workspace.monitor().context("No monitor")?;
  // Validate geometry before any mutation. No OS round trips occur during the tree swap.
  workspace.to_rect()?;
  release_scratchpad(workspace, state, config)?;
  let saved = Workspace::new(WorkspaceConfig {
    name: format!("__saved_{}", uuid::Uuid::new_v4()), display_name: None,
    bind_to_monitor: None, keep_alive: true,
  }, config.value.gaps.clone(), workspace.tiling_direction());
  saved.set_saved_workspace(Some(metadata(workspace.config().name)));
  attach_container(&saved.clone().into(), &monitor.into(), None)?;
  workspace.swap_contents(&saved);
  set_focused_descendant(&workspace.clone().into(), None);
  state.pending_sync.queue_focus_change();
  state.emit_event(WmEvent::WorkspaceActivated { activated_workspace: saved.to_dto()? });
  changed(&saved, state)?;
  changed(workspace, state)
}

pub fn restore_workspace(saved: &Workspace, name: &str, state: &mut WmState, config: &UserConfig) -> anyhow::Result<()> {
  ensure!(saved.is_saved(), "This workspace is no longer saved.");
  ensure!(config.value.workspaces.iter().any(|w| w.name == name), "Choose a configured workspace number.");
  saved.to_rect()?;
  if state.workspace_by_name(name).is_none() { activate_workspace(Some(name), None, state, config)?; }
  let target = state.workspace_by_name(name).context("Destination unavailable")?;
  ensure!(!target.is_saved() && target.id() != saved.id(), "Invalid destination.");
  let source_monitor = saved.monitor().context("No source monitor")?;
  let target_monitor = target.monitor().context("No target monitor")?;
  let target_rect = target.to_rect()?;
  release_scratchpad(&target, state, config)?;
  let occupied = target.has_children();
  // Give displaced contents a fresh identity. A stale picker/duplicate restore
  // must fail, not restore a different workspace that reused the selected ID.
  let displaced = if occupied {
    let ws = Workspace::new(WorkspaceConfig {
      name: format!("__saved_{}", uuid::Uuid::new_v4()), display_name: None,
      bind_to_monitor: None, keep_alive: true,
    }, config.value.gaps.clone(), target.tiling_direction());
    ws.set_saved_workspace(Some(metadata(name.to_owned())));
    attach_container(&ws.clone().into(), &target_monitor.clone().into(), None)?;
    target.swap_contents(&ws);
    Some(ws)
  } else { None };
  target.swap_contents(saved);
  if source_monitor.id() != target_monitor.id() {
      for window in target.descendants().filter_map(|c| c.as_window_container().ok()) {
        window.set_has_pending_dpi_adjustment(true);
        window.set_floating_placement(window.floating_placement().translate_to_center(&target_rect));
      }
  }
  let focus = target.descendant_focus_order().next().unwrap_or_else(|| target.clone().into());
  set_focused_descendant(&focus, None);
  state.pending_sync.queue_focus_change().queue_container_to_redraw(target_monitor);
  changed(&target, state)?;
  if let Some(displaced) = displaced {
    state.emit_event(WmEvent::WorkspaceActivated { activated_workspace: displaced.to_dto()? });
    changed(&displaced, state)?;
  }
  deactivate_workspace(saved.clone(), state)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{models::SplitContainer, traits::TilingSizeGetters};
  use wm_common::{GapsConfig, TilingDirection};
  fn workspace(name: &str) -> Workspace {
    Workspace::new(WorkspaceConfig { name: name.into(), display_name: Some(name.into()), bind_to_monitor: None, keep_alive: true },
      GapsConfig::default(), TilingDirection::Horizontal)
  }
  #[test]
  fn intact_swap_preserves_nested_ratios_parents_and_focus_mode() {
    let a = workspace("3"); let b = workspace("saved");
    let split = SplitContainer::new(TilingDirection::Vertical, GapsConfig::default());
    let nested = SplitContainer::new(TilingDirection::Horizontal, GapsConfig::default());
    attach_container(&split.clone().into(), &a.clone().into(), None).unwrap();
    attach_container(&nested.clone().into(), &split.clone().into(), None).unwrap();
    nested.set_tiling_size(0.37);
    a.set_focus_mode(true);
    let order = a.borrow_child_focus_order().clone();
    a.swap_contents(&b);
    assert!(!a.has_children()); assert!(b.focus_mode()); assert!(!a.focus_mode());
    assert_eq!(nested.tiling_size(),0.37);
    assert_eq!(nested.workspace().unwrap().id(), b.id());
    assert_eq!(*b.borrow_child_focus_order(),order);
    a.swap_contents(&b);
    assert_eq!(nested.workspace().unwrap().id(), a.id());
    assert_eq!(a.config().display_name.as_deref(),Some("3"));
    assert!(a.focus_mode());
  }
  #[test]
  fn tiling_changes_drop_the_lone_window_shrink() {
    let a = workspace("3"); let b = workspace("saved");
    a.set_lone_size(Some((800, 600)));
    a.swap_contents(&b);
    assert_eq!(b.lone_size(), Some((800, 600))); assert_eq!(a.lone_size(), None);
    let split = SplitContainer::new(TilingDirection::Vertical, GapsConfig::default());
    attach_container(&split.clone().into(), &b.clone().into(), None).unwrap();
    assert_eq!(b.lone_size(), None);
    assert!(b.lone_tiling_window().is_none(), "a split is not a lone window");
    b.set_lone_size(Some((800, 600)));
    crate::commands::container::detach_container(split.into()).unwrap();
    assert_eq!(b.lone_size(), None);
  }
  #[test]
  fn saved_subtrees_are_not_focus_targets() {
    let saved = workspace("saved");
    let split = SplitContainer::new(TilingDirection::Vertical, GapsConfig::default());
    attach_container(&split.into(), &saved.clone().into(), None).unwrap();
    assert!(saved.descendant_focus_order().next().is_some());
    saved.set_saved_workspace(Some(metadata("3".into())));
    assert!(saved.descendant_focus_order().next().is_none());
    assert!(!saved.is_displayed());
  }
}
