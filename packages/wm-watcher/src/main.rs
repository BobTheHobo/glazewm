#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
  tracing_subscriber::fmt().init();
  let pid = std::env::args().nth(1)
    .ok_or_else(|| anyhow::anyhow!("Expected manager process ID"))?.parse::<u32>()?;
  // Normal exit is also checked: an exit notification is not proof that every
  // window was restored. Successfully restored records have already been removed.
  // Recovery matches the manager's PID and start time while its handle is held.
  let restored = wm_platform::watch_manager(pid, || println!("ready"))?;
  tracing::info!("Recovered {restored} windows after manager {pid} exited");
  Ok(())
}
