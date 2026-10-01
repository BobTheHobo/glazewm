use tracing::info;
use wm_common::{DisplayState, HideMethod};
use wm_platform::NativeWindow;

use crate::{
  commands::window::unmanage_window, traits::WindowGetters,
  user_config::UserConfig, wm_state::WmState,
};

pub fn handle_window_hidden(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);

  if let Some(window) = found_window {
    info!("Window hidden: {window}");
    crate::tracking_log::managed(&window, "hidden-event");

    // Update the display state.
    if config.value.general.hide_method != HideMethod::PlaceInCorner
      && window.display_state() == DisplayState::Hiding
    {
      window.set_display_state(DisplayState::Hidden);
      crate::tracking_log::managed(&window, "hide-transition-complete");
      return Ok(());
    }

    // Unmanage the window if it's not in a display state transition. Also,
    // since window events are not 100% guaranteed to be in correct order,
    // we need to ignore events where the window is not actually hidden.
    if config.value.general.hide_method == HideMethod::PlaceInCorner
      || window.display_state() == DisplayState::Shown
    {
      let visibility = window.native().is_visible();
      tracing::info!(target: "window_tracking", hwnd = ?window.native().id(),
        result = ?visibility, "hidden-visibility-decision");
      if !visibility.unwrap_or(false) {
        unmanage_window(window, state, "unmanage-hidden-while-shown")?;
      }
    }
  }

  Ok(())
}
