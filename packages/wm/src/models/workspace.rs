use std::{
  cell::{Ref, RefCell, RefMut},
  collections::VecDeque,
  rc::Rc,
};

use anyhow::Context;
use uuid::Uuid;
use wm_common::{
  ContainerDto, GapsConfig, TilingDirection, WorkspaceConfig, WorkspaceDto,
};
use wm_platform::{Rect, RectDelta};

use crate::{
  impl_common_getters, impl_container_debug,
  impl_tiling_direction_getters,
  models::{
    Container, DirectionContainer, TilingContainer, TilingWindow,
    WindowContainer,
  },
  traits::{CommonGetters, PositionGetters, TilingDirectionGetters},
};

#[derive(Clone)]
pub struct Workspace(Rc<RefCell<WorkspaceInner>>);

#[derive(Debug)]
struct WorkspaceInner {
  id: Uuid,
  parent: Option<Container>,
  children: VecDeque<Container>,
  child_focus_order: VecDeque<Uuid>,
  config: WorkspaceConfig,
  gaps_config: GapsConfig,
  tiling_direction: TilingDirection,
  focus_mode: bool,
  /// Shrunk size of a tiling window that has this workspace to itself
  /// (resize mode), shown centred. Dropped when a tiling window joins or leaves.
  lone_size: Option<(i32, i32)>,
  saved_workspace: Option<wm_common::SavedWorkspaceDto>,
  scratchpad: Option<wm_common::ScratchpadDto>,
  scratchpad_hint_at: u64,
}

impl Workspace {
  pub fn new(
    config: WorkspaceConfig,
    gaps_config: GapsConfig,
    tiling_direction: TilingDirection,
  ) -> Self {
    let workspace = WorkspaceInner {
      id: Uuid::new_v4(),
      parent: None,
      children: VecDeque::new(),
      child_focus_order: VecDeque::new(),
      config,
      gaps_config,
      tiling_direction,
      focus_mode: false,
      lone_size: None,
      saved_workspace: None,
      scratchpad: None,
      scratchpad_hint_at: 0,
    };

    Self(Rc::new(RefCell::new(workspace)))
  }

  pub fn show_scratchpad_hint(&self) { self.0.borrow_mut().scratchpad_hint_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64; }

  pub fn scratchpad(&self) -> Option<wm_common::ScratchpadDto> { self.0.borrow().scratchpad.clone() }
  pub fn set_scratchpad(&self, value: Option<wm_common::ScratchpadDto>) { self.0.borrow_mut().scratchpad = value; }

  pub fn saved_workspace(&self) -> Option<wm_common::SavedWorkspaceDto> { self.0.borrow().saved_workspace.clone() }
  pub fn set_saved_workspace(&self, saved: Option<wm_common::SavedWorkspaceDto>) { self.0.borrow_mut().saved_workspace = saved; }
  pub fn is_saved(&self) -> bool { self.0.borrow().saved_workspace.is_some() }

  /// Swap intact layout trees. Per-window moves would flatten splits and reset sizes.
  pub fn swap_contents(&self, other: &Self) {
    assert!(self.id() != other.id());
    {
      let mut a = self.0.borrow_mut();
      let mut b = other.0.borrow_mut();
      std::mem::swap(&mut a.children, &mut b.children);
      std::mem::swap(&mut a.child_focus_order, &mut b.child_focus_order);
      std::mem::swap(&mut a.tiling_direction, &mut b.tiling_direction);
      std::mem::swap(&mut a.focus_mode, &mut b.focus_mode);
      std::mem::swap(&mut a.lone_size, &mut b.lone_size);
      std::mem::swap(&mut a.config.display_name, &mut b.config.display_name);
    }
    for ws in [self, other] {
      for child in ws.children() { *child.borrow_parent_mut() = Some(ws.clone().into()); }
    }
  }

  pub fn focus_mode(&self) -> bool { self.0.borrow().focus_mode }

  pub fn set_focus_mode(&self, enabled: bool) { self.0.borrow_mut().focus_mode = enabled; }

  pub fn lone_size(&self) -> Option<(i32, i32)> { self.0.borrow().lone_size }

  pub fn set_lone_size(&self, size: Option<(i32, i32)>) { self.0.borrow_mut().lone_size = size; }

  /// The tiling window that has this workspace to itself, if any.
  pub fn lone_tiling_window(&self) -> Option<TilingWindow> {
    let mut tiling = self.tiling_children();
    match (tiling.next(), tiling.next()) {
      (Some(TilingContainer::TilingWindow(window)), None) => Some(window),
      _ => None,
    }
  }

  /// Underlying config for the workspace.
  pub fn config(&self) -> WorkspaceConfig {
    self.0.borrow().config.clone()
  }

  /// Update the underlying config for the workspace.
  pub fn set_config(&self, config: WorkspaceConfig) {
    self.0.borrow_mut().config = config;
  }

  /// Whether the workspace is currently displayed by the parent monitor.
  pub fn is_displayed(&self) -> bool {
    if self.is_saved() { return false; }
    self
      .monitor()
      .and_then(|monitor| monitor.displayed_workspace())
      .is_some_and(|workspace| workspace.id() == self.id())
  }

  pub fn set_gaps_config(&self, gaps_config: GapsConfig) {
    self.0.borrow_mut().gaps_config = gaps_config;
  }

  /// Effective outer gaps for this workspace.
  ///
  /// Uses `single_window_outer_gap` when the workspace has a single tiling
  /// window, otherwise falls back to `outer_gap`.
  pub fn outer_gaps(&self) -> RectDelta {
    let is_single_window = self.tiling_children().nth(1).is_none();

    let gaps_config = &self.0.borrow().gaps_config;
    let gaps = if is_single_window {
      gaps_config
        .single_window_outer_gap
        .as_ref()
        .unwrap_or(&gaps_config.outer_gap)
    } else {
      &gaps_config.outer_gap
    };

    // TODO: Should this be scaled by the monitor's DPI?
    gaps.clone()
  }

  /// Gets the bounds of a workspace with the given outer gap config.
  fn workspace_rect_with_gap_config(
    &self,
    outer_gaps: &RectDelta,
  ) -> anyhow::Result<Rect> {
    let monitor =
      self.monitor().context("Workspace has no parent monitor.")?;

    let gaps_config = &self.0.borrow().gaps_config;
    let scale_factor = if gaps_config.scale_with_dpi {
      monitor.native_properties().scale_factor
    } else {
      1.
    };

    // Get the delta between the monitor's bounds and its working area.
    let monitor_bounds = monitor.native_properties().bounds;
    let working_area_delta = monitor
      .native_properties()
      .working_area
      .delta(&monitor_bounds);

    Ok(
      monitor_bounds
        // Scale the gaps if `scale_with_dpi` is enabled. Outer gap config
        // values can be a percentage (relative to the monitor bounds), so
        // the outer gap delta needs to be applied prior to the working
        // area delta.
        .apply_delta(&outer_gaps.inverse(), Some(scale_factor))
        .apply_delta(&working_area_delta, None),
    )
  }

  /// Gets the maximum bounds of a workspace considering both `outer_gap`
  /// and `single_window_outer_gap` config values.
  pub fn max_workspace_rect(&self) -> anyhow::Result<Rect> {
    let gaps_config = &self.0.borrow().gaps_config;

    // Get the workspace rect using `outer_gap`.
    let multi_window_rect =
      self.workspace_rect_with_gap_config(&gaps_config.outer_gap)?;

    let Some(single_gap) = &gaps_config.single_window_outer_gap else {
      return Ok(multi_window_rect);
    };

    // Get the workspace rect using `single_window_outer_gap`.
    let single_window_rect =
      self.workspace_rect_with_gap_config(single_gap)?;

    Ok(multi_window_rect.union(&single_window_rect))
  }

  pub fn to_dto(&self) -> anyhow::Result<ContainerDto> {
    let rect = self.to_rect()?;
    let config = self.config();

    let children = self
      .children()
      .iter()
      .map(CommonGetters::to_dto)
      .try_collect()?;

    Ok(ContainerDto::Workspace(WorkspaceDto {
      id: self.id(),
      name: config.name,
      display_name: config.display_name,
      parent_id: self.parent().map(|parent| parent.id()),
      children,
      child_focus_order: self.0.borrow().child_focus_order.clone().into(),
      has_focus: self.has_focus(None),
      is_displayed: self.is_displayed(),
      focus_mode: self.focus_mode(),
      saved_workspace: self.saved_workspace(),
      scratchpad: self.scratchpad(),
      scratchpad_hint_at: self.0.borrow().scratchpad_hint_at,
      width: rect.width(),
      height: rect.height(),
      x: rect.x(),
      y: rect.y(),
      tiling_direction: self.tiling_direction(),
    }))
  }
}

impl_container_debug!(Workspace);
impl_common_getters!(Workspace);
impl_tiling_direction_getters!(Workspace);

impl PositionGetters for Workspace {
  fn to_rect(&self) -> anyhow::Result<Rect> {
    self.workspace_rect_with_gap_config(&self.outer_gaps())
  }
}

impl std::fmt::Display for Workspace {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "Workspace(name={}, tiling_direction={:?})",
      self.config().name,
      self.tiling_direction(),
    )
  }
}
