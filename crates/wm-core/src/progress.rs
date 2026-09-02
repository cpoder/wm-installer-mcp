//! Progress a long job publishes, and a terminal rendering of it.
//!
//! A native install takes about four minutes. Until now the only feedback was
//! the tail of a log, which tells a human very little and gives an agent
//! nothing it can render. So a job writes `progress.json` beside its log as it
//! goes, and anything watching reads that.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// What a job is doing, as of its last update.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    /// What stage it is in, e.g. `downloading`, `tooling jars`.
    pub phase: String,
    /// Steps completed and expected.
    pub done: usize,
    pub total: usize,
    /// Bytes fetched, and how many the plan said to expect.
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// What it is working on right now.
    pub current: String,
    /// Unix seconds.
    pub started: u64,
    pub updated: u64,
    /// Set once the job stops: whether it succeeded.
    pub finished: Option<bool>,
    /// A closing line, or the failure.
    pub message: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Progress {
    /// Start a run of `total` steps.
    pub fn new(phase: &str, total: usize, bytes_total: u64) -> Self {
        let t = now();
        Self {
            phase: phase.to_string(),
            total,
            bytes_total,
            started: t,
            updated: t,
            ..Default::default()
        }
    }

    /// Where a job's progress lives.
    pub fn path(job_dir: &Path) -> PathBuf {
        job_dir.join("progress.json")
    }

    /// Publish. A failure to write must never fail the job it is describing.
    pub fn write(&self, job_dir: &Path) {
        if let Ok(text) = serde_json::to_string(self) {
            let _ = fs::write(Self::path(job_dir), text);
        }
    }

    /// Read what a job last published, if it published anything.
    pub fn read(job_dir: &Path) -> Option<Self> {
        let text = fs::read_to_string(Self::path(job_dir)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Record one completed step.
    pub fn step(&mut self, current: &str, bytes: u64, job_dir: &Path) {
        self.done += 1;
        self.bytes_done += bytes;
        // The plan's total is an estimate: the tree declares no size for
        // resource jars, so the real figure can exceed it. Revise the estimate
        // upward rather than render 816 MB of 812 MB.
        self.bytes_total = self.bytes_total.max(self.bytes_done);
        self.current = current.to_string();
        self.updated = now();
        self.write(job_dir);
    }

    /// Move to another stage, resetting the step counter.
    ///
    /// `extra_bytes` is what this stage adds to the expected total. Without it
    /// a later stage's bytes land in `bytes_done` against a total that never
    /// counted them, and the bar reads over 100%.
    pub fn phase(&mut self, phase: &str, total: usize, extra_bytes: u64, job_dir: &Path) {
        self.phase = phase.to_string();
        self.done = 0;
        self.total = total;
        self.bytes_total += extra_bytes;
        self.current.clear();
        self.updated = now();
        self.write(job_dir);
    }

    /// Close it out.
    pub fn finish(&mut self, ok: bool, message: &str, job_dir: &Path) {
        self.finished = Some(ok);
        self.message = Some(message.to_string());
        self.updated = now();
        self.write(job_dir);
    }

    /// Seconds since it started.
    pub fn elapsed(&self) -> u64 {
        self.updated.saturating_sub(self.started)
    }

    /// Fraction complete, by bytes where a size is known and by steps
    /// otherwise. Bytes are the better guide: artifacts differ by two orders of
    /// magnitude in size, so a step count runs far ahead of the real work.
    pub fn fraction(&self) -> f64 {
        if self.finished == Some(true) {
            return 1.0;
        }
        if self.bytes_total > 0 {
            return (self.bytes_done as f64 / self.bytes_total as f64).clamp(0.0, 1.0);
        }
        if self.total > 0 {
            return (self.done as f64 / self.total as f64).clamp(0.0, 1.0);
        }
        0.0
    }

    /// Seconds still to go, from the rate so far. `None` before there is
    /// enough of a sample to be worth showing.
    pub fn remaining(&self) -> Option<u64> {
        let fraction = self.fraction();
        let elapsed = self.elapsed();
        if fraction <= 0.02 || elapsed < 3 || self.finished.is_some() {
            return None;
        }
        let total = elapsed as f64 / fraction;
        Some((total - elapsed as f64).max(0.0) as u64)
    }

    /// Bytes per second so far.
    pub fn rate(&self) -> u64 {
        match self.elapsed() {
            0 => 0,
            seconds => self.bytes_done / seconds,
        }
    }

    /// One screen describing the run, for a terminal.
    pub fn render(&self, job_id: &str, width: usize) -> String {
        let bar_width = width.saturating_sub(8).clamp(10, 48);
        let filled = (self.fraction() * bar_width as f64).round() as usize;
        let bar: String = std::iter::repeat_n('█', filled)
            .chain(std::iter::repeat_n('░', bar_width - filled))
            .collect();

        let mut out = String::new();
        out.push_str(&format!("  {job_id}\n\n"));
        out.push_str(&format!("  {bar}  {:>3.0}%\n\n", self.fraction() * 100.0));
        out.push_str(&format!("  phase      {}\n", self.phase));
        if self.total > 0 {
            out.push_str(&format!("  step       {} of {}\n", self.done, self.total));
        }
        if self.bytes_total > 0 {
            out.push_str(&format!(
                "  fetched    {} of {}  ({}/s)\n",
                human_bytes(self.bytes_done),
                human_bytes(self.bytes_total),
                human_bytes(self.rate())
            ));
        }
        out.push_str(&format!("  elapsed    {}\n", human_time(self.elapsed())));
        match (self.finished, self.remaining()) {
            (Some(true), _) => out.push_str("  state      done\n"),
            (Some(false), _) => out.push_str("  state      FAILED\n"),
            (None, Some(left)) => {
                out.push_str(&format!("  remaining  about {}\n", human_time(left)))
            }
            (None, None) => out.push_str("  remaining  estimating…\n"),
        }
        if !self.current.is_empty() {
            out.push_str(&format!(
                "\n  {}\n",
                truncate(&self.current, width.saturating_sub(4))
            ));
        }
        if let Some(message) = &self.message {
            out.push_str(&format!(
                "\n  {}\n",
                truncate(message, width.saturating_sub(4))
            ));
        }
        out
    }
}

/// Bytes in the largest unit that keeps the number readable.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("kB", 1_000),
        ("B", 1),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let value = bytes as f64 / scale as f64;
            return if *unit == *"B" {
                format!("{bytes} B")
            } else if value < 10.0 {
                format!("{value:.2} {unit}")
            } else {
                format!("{value:.0} {unit}")
            };
        }
    }
    "0 B".to_string()
}

/// Seconds as `4m 21s`.
pub fn human_time(seconds: u64) -> String {
    match (seconds / 60, seconds % 60) {
        (0, s) => format!("{s}s"),
        (m, s) => format!("{m}m {s:02}s"),
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_scaled_to_something_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2_400), "2.40 kB");
        assert_eq!(human_bytes(820_000_000), "820 MB");
        assert_eq!(human_bytes(1_500_000_000), "1.50 GB");
    }

    #[test]
    fn time_reads_as_minutes_past_a_minute() {
        assert_eq!(human_time(9), "9s");
        assert_eq!(human_time(61), "1m 01s");
        assert_eq!(human_time(261), "4m 21s");
    }

    #[test]
    fn progress_prefers_bytes_over_steps() {
        // Artifacts differ by two orders of magnitude, so counting them runs
        // ahead of the work: 90 of 125 steps can be a third of the bytes.
        let mut p = Progress::new("downloading", 125, 1_000);
        p.done = 90;
        p.bytes_done = 300;
        assert!((p.fraction() - 0.3).abs() < 1e-9);
        // With no size to go on it falls back to steps.
        p.bytes_total = 0;
        assert!((p.fraction() - 0.72).abs() < 1e-9);
    }

    #[test]
    fn no_estimate_until_there_is_a_sample_worth_one() {
        let mut p = Progress::new("downloading", 100, 1_000);
        assert_eq!(p.remaining(), None, "nothing done yet");
        p.bytes_done = 10;
        p.updated = p.started + 1;
        assert_eq!(p.remaining(), None, "one second is not a sample");
        p.bytes_done = 500;
        p.updated = p.started + 10;
        assert_eq!(
            p.remaining(),
            Some(10),
            "half done in 10s means about 10s left"
        );
    }

    #[test]
    fn an_underestimated_total_is_revised_rather_than_exceeded() {
        let dir = std::path::Path::new("/nonexistent");
        let mut p = Progress::new("downloading", 2, 100);
        p.step("a", 80, dir);
        assert_eq!(p.bytes_total, 100, "still within the estimate");
        // The tree declares no size for resource jars, so the real total can
        // overshoot the plan. The bar must not read past 100%.
        p.step("b", 60, dir);
        assert_eq!(p.bytes_done, 140);
        assert_eq!(p.bytes_total, 140);
        assert_eq!(p.fraction(), 1.0);
    }

    #[test]
    fn a_later_phase_adds_its_bytes_to_the_total() {
        let dir = std::path::Path::new("/nonexistent");
        let mut p = Progress::new("downloading", 2, 100);
        p.bytes_done = 100;
        assert_eq!(p.fraction(), 1.0);
        // A second stage that fetches more must not push the bar past 100%.
        p.phase("tooling jars", 3, 50, dir);
        p.bytes_done += 50;
        assert_eq!(p.bytes_total, 150);
        assert_eq!(p.fraction(), 1.0);
        assert!(p.bytes_done <= p.bytes_total);
    }

    #[test]
    fn a_finished_run_is_complete_and_estimates_nothing() {
        let mut p = Progress::new("downloading", 10, 100);
        p.bytes_done = 40;
        p.updated = p.started + 5;
        p.finished = Some(true);
        assert_eq!(p.fraction(), 1.0);
        assert_eq!(p.remaining(), None);
    }
}
