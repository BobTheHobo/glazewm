use wm_platform::LengthValue;

use super::set_window_size;
use crate::{
  models::WindowContainer,
  traits::{
    CommonGetters, PositionGetters, TilingSizeGetters, WindowGetters,
  },
  wm_state::WmState,
};

/// Smallest size a lone tiling window can be shrunk to.
const MIN_LONE_WIDTH: i32 = 250;
const MIN_LONE_HEIGHT: i32 = 140;

pub fn resize_window(
  window: &WindowContainer,
  width_delta: Option<LengthValue>,
  height_delta: Option<LengthValue>,
  state: &mut WmState,
) -> anyhow::Result<()> {
  // A tiling window alone on its workspace has no siblings to trade space
  // with, so it shrinks or grows in place, centred (not in focus mode,
  // where it fills the workspace anyway).
  if let WindowContainer::TilingWindow(tiling_window) = window {
    if let Some(workspace) = tiling_window.workspace().filter(|ws| {
      !ws.focus_mode()
        && ws
          .lone_tiling_window()
          .is_some_and(|lone| lone.id() == tiling_window.id())
    }) {
      let full = tiling_window.to_rect()?;
      let current = tiling_window.presentation_rect()?;
      workspace.set_lone_size(lone_window_size(
        (current.width(), current.height()),
        (full.width(), full.height()),
        width_delta.map(|delta| delta.to_px(full.width(), None)),
        height_delta.map(|delta| delta.to_px(full.height(), None)),
      ));
      state.pending_sync.queue_container_to_redraw(tiling_window.clone());
      return Ok(());
    }
  }

  let window_rect = window.to_rect()?;

  let target_width = match width_delta {
    Some(delta) => {
      let parent_width = match window.as_tiling_container() {
        Ok(tiling_window) => tiling_window
          .container_to_resize(true)?
          .and_then(|container| container.parent())
          .and_then(|parent| {
            parent.to_rect().ok().map(|rect| rect.width())
          })
          .and_then(|parent_width| {
            let (horizontal_gap, _) = tiling_window.inner_gaps().ok()?;

            #[allow(
              clippy::cast_possible_wrap,
              clippy::cast_possible_truncation
            )]
            Some(
              parent_width
                - horizontal_gap
                  * tiling_window.tiling_siblings().count() as i32,
            )
          }),
        _ => window.parent().and_then(|parent| {
          parent.to_rect().ok().map(|rect| rect.width())
        }),
      };

      parent_width.map(|parent_width| {
        window_rect.width() + delta.to_px(parent_width, None)
      })
    }
    _ => None,
  };

  let target_height = match height_delta {
    Some(delta) => {
      let parent_height = match window.as_tiling_container() {
        Ok(tiling_window) => tiling_window
          .container_to_resize(false)?
          .and_then(|container| container.parent())
          .and_then(|parent| {
            parent.to_rect().ok().map(|rect| rect.height())
          })
          .and_then(|parent_height| {
            let (_, vertical_gap) = tiling_window.inner_gaps().ok()?;

            #[allow(
              clippy::cast_possible_wrap,
              clippy::cast_possible_truncation
            )]
            Some(
              parent_height
                - vertical_gap
                  * tiling_window.tiling_siblings().count() as i32,
            )
          }),
        _ => window.parent().and_then(|parent| {
          parent.to_rect().ok().map(|rect| rect.height())
        }),
      };

      parent_height.map(|parent_height| {
        window_rect.height() + delta.to_px(parent_height, None)
      })
    }
    _ => None,
  };

  set_window_size(
    window.clone(),
    target_width.map(LengthValue::from_px),
    target_height.map(LengthValue::from_px),
    state,
  )?;

  Ok(())
}

/// New size of a lone tiling window after a resize, or `None` once it fills
/// the workspace again in both directions.
fn lone_window_size(
  current: (i32, i32),
  full: (i32, i32),
  width_delta: Option<i32>,
  height_delta: Option<i32>,
) -> Option<(i32, i32)> {
  let apply = |length: i32, delta: Option<i32>, min: i32, max: i32| {
    delta.map_or(length, |delta| (length + delta).clamp(min.min(max), max))
  };
  let width = apply(current.0, width_delta, MIN_LONE_WIDTH, full.0);
  let height = apply(current.1, height_delta, MIN_LONE_HEIGHT, full.1);
  (width < full.0 || height < full.1).then_some((width, height))
}

#[cfg(test)]
mod tests {
  use wm_platform::Rect;

  use super::lone_window_size;
  use crate::traits::centered_in;

  #[test]
  fn lone_window_shrinks_grows_and_clamps() {
    let full = (1880, 1000);
    assert_eq!(lone_window_size(full, full, Some(-38), None), Some((1842, 1000)));
    assert_eq!(lone_window_size((1842, 1000), full, None, Some(-20)), Some((1842, 980)));
    assert_eq!(lone_window_size((300, 1000), full, Some(-500), None), Some((250, 1000)));
    // Growing back to full size in both directions drops the shrink.
    assert_eq!(lone_window_size((1842, 1000), full, Some(100), None), None);
    // A workspace narrower than the minimum can't be shrunk below itself.
    assert_eq!(lone_window_size((200, 100), (200, 100), Some(-10), Some(-10)), None);
  }

  #[test]
  fn shrunk_lone_window_stays_centred() {
    let outer = Rect::from_xy(20, 60, 1880, 1000);
    let rect = centered_in(&outer, (1000, 600));
    assert_eq!((rect.x(), rect.y(), rect.width(), rect.height()), (460, 260, 1000, 600));
    let capped = centered_in(&outer, (5000, 600));
    assert_eq!((capped.x(), capped.width()), (20, 1880));
  }
}
