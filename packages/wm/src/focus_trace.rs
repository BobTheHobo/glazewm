//! Foreground-change flight recorder (`window_tracking` target, no titles).
//! Answers "who took the foreground, and was it GlazeWM or someone else?".
use std::{borrow::Cow, time::Instant};

const BUCKET_MS: u64 = 1000;
/// A bucket with more foreground changes than this gets one rate summary.
pub const RATE_THRESHOLD: u32 = 4;
/// Detail lines logged per bucket; the rest are counted, not written.
pub const DETAIL_PER_BUCKET: u32 = 8;
/// A focus request from the WM explains a foreground change this soon after.
const WM_TARGET_MS: u64 = 1000;
const MAX_DISTINCT: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
  pub count: u32,
  pub first: (isize, u32),
  pub second: Option<(isize, u32)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
  /// Foreground changes so far in the current 1 s bucket (including this one).
  pub n: u32,
  pub log_detail: bool,
  /// Detail lines withheld in the previous bucket (reported on its successor).
  pub dropped_prev: u32,
  /// Set once per bucket, when `n` first exceeds [`RATE_THRESHOLD`].
  pub summary: Option<Summary>,
}

/// Tumbling 1 s buckets: bounds the detail written and spots ping-pong.
#[derive(Default)]
pub struct FocusGate {
  bucket_start: u64,
  count: u32,
  dropped_prev: u32,
  seen: Vec<(isize, u32)>,
}

impl FocusGate {
  pub fn record(&mut self, now_ms: u64, hwnd: isize) -> Decision {
    if self.count == 0 || now_ms.saturating_sub(self.bucket_start) >= BUCKET_MS {
      self.dropped_prev = self.count.saturating_sub(DETAIL_PER_BUCKET);
      self.bucket_start = now_ms;
      self.count = 0;
      self.seen.clear();
    }
    self.count += 1;
    if let Some(entry) = self.seen.iter_mut().find(|(h, _)| *h == hwnd) {
      entry.1 += 1;
    } else if self.seen.len() < MAX_DISTINCT {
      self.seen.push((hwnd, 1));
    }
    let summary = (self.count == RATE_THRESHOLD + 1).then(|| {
      let mut top = self.seen.clone();
      top.sort_by(|a, b| b.1.cmp(&a.1));
      Summary { count: self.count, first: top[0], second: top.get(1).copied() }
    });
    let dropped_prev = std::mem::take(&mut self.dropped_prev);
    Decision { n: self.count, log_detail: self.count <= DETAIL_PER_BUCKET, dropped_prev, summary }
  }
}

pub struct FocusTrace {
  start: Instant,
  gate: FocusGate,
  request_gate: FocusGate,
  pub last_hwnd: isize,
  wm_target: Option<(isize, u64)>,
  action: Cow<'static, str>,
  action_ms: u64,
}

impl Default for FocusTrace {
  fn default() -> Self {
    Self {
      start: Instant::now(), gate: FocusGate::default(), request_gate: FocusGate::default(),
      last_hwnd: 0, wm_target: None, action: Cow::Borrowed("none"), action_ms: 0,
    }
  }
}

impl FocusTrace {
  fn now(&self) -> u64 { self.start.elapsed().as_millis() as u64 }

  /// Remember the command or event being processed (the likely cause).
  pub fn note(&mut self, action: impl Into<Cow<'static, str>>) {
    self.action = action.into();
    self.action_ms = self.now();
  }

  /// The WM asked the OS to focus `hwnd` (0 = desktop window).
  pub fn wm_requested(&mut self, hwnd: isize) {
    let now = self.now();
    self.wm_target = (hwnd != 0).then_some((hwnd, now));
    let decision = self.request_gate.record(now, hwnd);
    if decision.log_detail {
      tracing::info!(target: "window_tracking", hwnd, after = self.action.as_ref(),
        after_ms = now.saturating_sub(self.action_ms), "focus-request");
    }
  }

  /// "wm" if the WM just asked for this window (consumed), else "external".
  fn classify(&mut self, hwnd: isize, now: u64) -> &'static str {
    match self.wm_target {
      Some((target, at)) if target == hwnd && now.saturating_sub(at) <= WM_TARGET_MS => {
        self.wm_target = None;
        "wm"
      }
      _ => "external",
    }
  }

  /// Log one EVENT_SYSTEM_FOREGROUND. `describe` yields (pid, process,
  /// workspace) lazily so suppressed events cost nothing.
  pub fn foreground(&mut self, hwnd: isize, synced: bool, describe: impl FnOnce() -> (u32, String, String)) {
    let now = self.now();
    let initiator = self.classify(hwnd, now);
    let decision = self.gate.record(now, hwnd);
    let previous = std::mem::replace(&mut self.last_hwnd, hwnd);
    if let Some(s) = decision.summary {
      let (b, bn) = s.second.unwrap_or((0, 0));
      tracing::info!(target: "window_tracking", count = s.count, hwnd_a = s.first.0, a_n = s.first.1,
        hwnd_b = b, b_n = bn, "foreground-rate");
    }
    if decision.log_detail {
      let (pid, process, workspace) = describe();
      tracing::info!(target: "window_tracking", hwnd, pid, process = %process, workspace = %workspace,
        initiator, previous, synced, n = decision.n, dropped_prev = decision.dropped_prev,
        last_action = self.action.as_ref(), last_action_ms = now.saturating_sub(self.action_ms),
        "foreground");
    }
  }
}

/// First word of a command's debug text, e.g. `Focus(..)` -> `Focus`.
pub fn command_name(debug: &str) -> String {
  debug.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or("?").to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ping_pong_gets_one_summary_naming_the_pair() {
    let mut gate = FocusGate::default();
    let mut summaries = vec![];
    for i in 0..20u64 {
      let d = gate.record(i * 40, if i % 2 == 0 { 111 } else { 222 });
      if let Some(s) = d.summary { summaries.push(s); }
    }
    assert_eq!(summaries.len(), 1);
    let s = &summaries[0];
    assert_eq!(s.count, 5);
    assert_eq!(s.first, (111, 3));
    assert_eq!(s.second, Some((222, 2)));
  }

  #[test]
  fn slow_changes_never_summarise() {
    let mut gate = FocusGate::default();
    for i in 0..50u64 {
      let d = gate.record(i * 300, i as isize % 3);
      assert!(d.summary.is_none() && d.log_detail);
    }
  }

  #[test]
  fn detail_is_capped_and_drops_reported_on_next_bucket() {
    let mut gate = FocusGate::default();
    let logged = (0..30u64).filter(|i| gate.record(i * 10, 1).log_detail).count();
    assert_eq!(logged as u32, DETAIL_PER_BUCKET);
    let next = gate.record(1500, 2);
    assert_eq!((next.n, next.log_detail, next.dropped_prev), (1, true, 30 - DETAIL_PER_BUCKET));
    assert_eq!(gate.record(1600, 2).dropped_prev, 0);
  }

  #[test]
  fn wm_requests_explain_one_matching_change_only() {
    let mut trace = FocusTrace::default();
    trace.wm_target = Some((7, 100));
    assert_eq!(trace.classify(8, 150), "external");
    assert_eq!(trace.classify(7, 150), "wm");
    assert_eq!(trace.classify(7, 160), "external");
    trace.wm_target = Some((7, 100));
    assert_eq!(trace.classify(7, 1200), "external");
  }

  #[test]
  fn many_distinct_windows_stay_bounded() {
    let mut gate = FocusGate::default();
    for i in 0..100 { gate.record(i, i as isize); }
    assert!(gate.seen.len() <= MAX_DISTINCT);
  }

  #[test]
  fn command_names_are_trimmed() {
    assert_eq!(command_name("Focus(FocusCommand { .. })"), "Focus");
    assert_eq!(command_name("WmReloadConfig"), "WmReloadConfig");
    assert_eq!(command_name("SetFloating { centered: true }"), "SetFloating");
  }
}
