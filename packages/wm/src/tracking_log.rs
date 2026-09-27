//! Small, title-free flight recorder. Disk writes never block the WM event loop.
use std::{fs::{File, OpenOptions}, io::{self, Write}, path::PathBuf};
use crate::{models::WindowContainer, traits::{CommonGetters, WindowGetters}};

pub struct RotatingWriter {
  path: PathBuf,
  file: Option<File>,
  size: u64,
  limit: u64,
}

impl RotatingWriter {
  pub fn new(path: PathBuf, limit: u64) -> io::Result<Self> {
    if limit == 0 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero log limit")); }
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let size = file.metadata()?.len();
    Ok(Self { path, file: Some(file), size, limit })
  }
  fn rotate(&mut self) -> io::Result<()> {
    self.file.take();
    let backup = |n| self.path.with_extension(format!("log.{n}"));
    for n in (1..=3).rev() {
      let from = if n == 1 { self.path.clone() } else { backup(n - 1) };
      let to = backup(n);
      if to.exists() { std::fs::remove_file(&to)?; }
      if from.exists() { std::fs::rename(from, to)?; }
    }
    self.file = Some(OpenOptions::new().create(true).append(true).open(&self.path)?);
    self.size = 0;
    Ok(())
  }
}
impl Write for RotatingWriter {
  fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    // Also split an oversized write to enforce the bound during an event storm.
    let mut rest = bytes;
    while !rest.is_empty() {
      if self.size >= self.limit || self.file.is_none() { self.rotate()?; }
      let count = rest.len().min((self.limit - self.size) as usize);
      self.file.as_mut().unwrap().write_all(&rest[..count])?;
      self.size += count as u64;
      rest = &rest[count..];
    }
    Ok(bytes.len())
  }
  fn flush(&mut self) -> io::Result<()> {
    if let Some(file) = &mut self.file { file.flush()?; }
    Ok(())
  }
}

pub fn managed(window: &WindowContainer, action: &'static str) {
  tracing::info!(target: "window_tracking", action, hwnd = ?window.native().id(),
    id = %window.id(), process = %window.native_properties().process_name,
    display = ?window.display_state(), state = ?window.state(),
    workspace = ?window.workspace().map(|w| w.id()), "managed-state");
  #[cfg(target_os = "windows")]
  wm_platform::trace_window_tracking(&window.native(), action);
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn tracking_rotation_is_bounded_and_survives_reopen() {
    let dir = std::env::temp_dir().join(format!("tracking-log-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("window-tracking.log");
    { let mut writer = RotatingWriter::new(path.clone(), 64).unwrap();
      writer.write_all(&[b'a'; 500]).unwrap(); }
    { let mut writer = RotatingWriter::new(path.clone(), 64).unwrap();
      writer.write_all(b"restart\n").unwrap(); }
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(Result::unwrap).collect();
    assert_eq!(files.len(), 4);
    assert!(files.iter().all(|f| f.metadata().unwrap().len() <= 64));
    assert!(std::fs::read_to_string(path).unwrap().ends_with("restart\n"));
    std::fs::remove_dir_all(dir).unwrap();
  }
}
