use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ContainerDto;
use crate::{TilingDirection, WindowState};
use wm_platform::Rect;

/// User-friendly representation of a workspace.
///
/// Used for IPC and debug logging.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDto {
  pub id: Uuid,
  pub name: String,
  pub display_name: Option<String>,
  pub parent_id: Option<Uuid>,
  pub children: Vec<ContainerDto>,
  pub child_focus_order: Vec<Uuid>,
  pub has_focus: bool,
  pub is_displayed: bool,
  #[serde(default)]
  pub focus_mode: bool,
  #[serde(default)]
  pub saved_workspace: Option<SavedWorkspaceDto>,
  #[serde(default)]
  pub scratchpad: Option<ScratchpadDto>,
  #[serde(default)]
  pub scratchpad_hint_at: u64,
  pub width: i32,
  pub height: i32,
  pub x: i32,
  pub y: i32,
  pub tiling_direction: TilingDirection,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScratchpadDto {
  pub id: Uuid,
  pub hidden: bool,
  pub title: String,
  pub process_name: String,
  #[serde(default)]
  pub restore_state: Option<ScratchpadRestoreState>,
}

/// The release target and overlay presentation are deliberately independent.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScratchpadRestoreState {
  pub original_state: WindowState,
  pub original_placement: Rect,
  pub overlay_placement: Rect,
  pub hidden: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedWorkspaceDto {
  pub original_name: String,
  pub saved_at: u64,
}
