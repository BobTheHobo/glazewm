use anyhow::Context;
use uuid::Uuid;

use super::set_focused_descendant;
use crate::{
  commands::workspace::focus_workspace, models::WorkspaceTarget,
  traits::CommonGetters, user_config::UserConfig, wm_state::WmState,
};

pub fn focus_container_by_id(
  container_id: &Uuid,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let focus_target = state
    .container_by_id(*container_id)
    .context("No container with given id")?;

  let workspace = focus_target.workspace();
  anyhow::ensure!(!workspace.as_ref().is_some_and(|ws| ws.is_saved()), "Restore this workspace first.");

  // A container on a hidden workspace: switch to that workspace first, the
  // way a manually focused off-screen window does. Setting focus alone
  // would make it the displayed workspace without redrawing, so the old
  // workspace's windows would stay on screen and the target stay hidden.
  if let Some(workspace) = workspace.filter(|ws| !ws.is_displayed()) {
    focus_workspace(
      WorkspaceTarget::Name(workspace.config().name),
      state,
      config,
    )?;
  }

  // Set focus to the target container.
  set_focused_descendant(&focus_target, None);
  state.pending_sync.queue_focus_change().queue_cursor_jump();

  Ok(())
}
