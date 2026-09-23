use wm_platform::{NativeWindow, NativeWindowWindowsExt};
fn restore(node: &serde_json::Value) {
  if node["type"] == "window" {
    if let Some(handle) = node["handle"].as_i64() {
      let window = NativeWindow::from_handle(handle as isize);
      if window.title().ok().as_deref() == node["title"].as_str()
        && window.process_name().ok().as_deref() == node["processName"].as_str() {
        let _ = window.set_cloaked(false);
        let _ = window.show();
      }
    }
  }
  if let Some(children) = node["children"].as_array() {
    for child in children { restore(child); }
  }
}
fn main() -> anyhow::Result<()> {
  let file = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("Expected saved workspace query"))?;
  let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file)?)?;
  for workspace in value["data"]["workspaces"].as_array().ok_or_else(|| anyhow::anyhow!("Invalid workspace query"))? { restore(workspace); }
  Ok(())
}
