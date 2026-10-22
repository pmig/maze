//! Startup and frame timing (PLAN §8: cold start < 300 ms, 120 fps scroll).
//!
//! - Startup: from the process start instant the binary passes in to the first frame
//!   that painted the message list, logged once on the `mazemail::perf` target.
//! - Frames: the CPU time of each frame, from the start of the window's render to the
//!   end of its paint, plus the interval between painted frames.
//! - A scripted scroll benchmark ([`ScrollBenchmark`]) scrolls the message list at a
//!   constant speed for a number of frames and logs percentiles.

use std::time::{Duration, Instant};

/// Configuration of the scripted scroll benchmark.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollBenchmark {
    /// Frames to scroll.
    pub frames: usize,
    /// Pixels per frame.
    pub pixels_per_frame: f32,
    /// Use the table view instead of cards.
    pub table: bool,
    /// Quit when done.
    pub exit: bool,
}

impl ScrollBenchmark {
    /// Reads `MAZEMAIL_BENCH_SCROLL=<frames>`, `MAZEMAIL_BENCH_VIEW=table`,
    /// `MAZEMAIL_BENCH_SPEED=<px per frame>` and `MAZEMAIL_BENCH_EXIT=1`.
    pub fn from_env() -> Option<Self> {
        let frames = std::env::var("MAZEMAIL_BENCH_SCROLL").ok()?.parse().ok()?;
        Some(Self {
            frames,
            pixels_per_frame: std::env::var("MAZEMAIL_BENCH_SPEED")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(40.),
            table: std::env::var("MAZEMAIL_BENCH_VIEW").is_ok_and(|v| v == "table"),
            exit: std::env::var("MAZEMAIL_BENCH_EXIT").is_ok_and(|v| v == "1"),
        })
    }
}

/// Percentiles of a set of durations, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Percentiles {
    /// Number of samples.
    pub count: usize,
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Maximum.
    pub max: f64,
}

impl Percentiles {
    /// Percentiles of `samples` (nearest-rank). `None` for no samples.
    pub fn of(samples: &[Duration]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1e3).collect();
        ms.sort_by(f64::total_cmp);
        let rank = |p: f64| ms[((p * ms.len() as f64).ceil() as usize).clamp(1, ms.len()) - 1];
        Some(Self {
            count: ms.len(),
            p50: rank(0.50),
            p95: rank(0.95),
            p99: rank(0.99),
            max: ms[ms.len() - 1],
        })
    }
}

/// Collects frame timings for the window.
#[derive(Debug)]
pub struct FrameStats {
    process_start: Instant,
    startup_logged: bool,
    render_started: Option<Instant>,
    last_paint: Option<Instant>,
    recording: bool,
    cpu: Vec<Duration>,
    intervals: Vec<Duration>,
}

impl FrameStats {
    /// Stats relative to the process start.
    pub fn new(process_start: Instant) -> Self {
        Self {
            process_start,
            startup_logged: false,
            render_started: None,
            last_paint: None,
            recording: false,
            cpu: Vec::new(),
            intervals: Vec::new(),
        }
    }

    /// Call at the start of the window's render.
    pub fn render_started(&mut self) {
        self.render_started = Some(Instant::now());
    }

    /// Call at the end of paint. Returns the startup time on the first frame that
    /// showed list rows.
    pub fn painted(&mut self, list_has_rows: bool) -> Option<Duration> {
        let now = Instant::now();
        if self.recording {
            if let Some(start) = self.render_started {
                self.cpu.push(now - start);
            }
            if let Some(last) = self.last_paint {
                self.intervals.push(now - last);
            }
        }
        self.last_paint = Some(now);
        if !self.startup_logged && list_has_rows {
            self.startup_logged = true;
            let startup = now - self.process_start;
            tracing::info!(
                target: "mazemail::perf",
                first_frame_ms = startup.as_secs_f64() * 1e3,
                "first frame with the message list"
            );
            return Some(startup);
        }
        None
    }

    /// Starts recording frame samples.
    pub fn start_recording(&mut self) {
        self.recording = true;
        self.cpu.clear();
        self.intervals.clear();
        self.last_paint = None;
    }

    /// Stops recording and returns (CPU time, frame interval) percentiles.
    pub fn stop_recording(&mut self) -> (Option<Percentiles>, Option<Percentiles>) {
        self.recording = false;
        (Percentiles::of(&self.cpu), Percentiles::of(&self.intervals))
    }

    /// Frames recorded so far.
    pub fn recorded_frames(&self) -> usize {
        self.cpu.len()
    }
}

/// Resident memory of this process in bytes, if the platform tells.
pub fn resident_memory() -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let kib: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(kib * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples: Vec<_> = (1..=100).map(Duration::from_millis).collect();
        let p = Percentiles::of(&samples).unwrap();
        assert_eq!(p.count, 100);
        assert!((p.p50 - 50.).abs() < 1e-9);
        assert!((p.p95 - 95.).abs() < 1e-9);
        assert!((p.p99 - 99.).abs() < 1e-9);
        assert!((p.max - 100.).abs() < 1e-9);
        assert!(Percentiles::of(&[]).is_none());
    }

    #[test]
    fn startup_is_reported_once_rows_paint() {
        let mut stats = FrameStats::new(Instant::now());
        stats.render_started();
        assert!(stats.painted(false).is_none());
        stats.render_started();
        assert!(stats.painted(true).is_some());
        assert!(stats.painted(true).is_none());

        stats.start_recording();
        for _ in 0..3 {
            stats.render_started();
            stats.painted(true);
        }
        assert_eq!(stats.recorded_frames(), 3);
        let (cpu, intervals) = stats.stop_recording();
        assert_eq!(cpu.unwrap().count, 3);
        assert_eq!(intervals.unwrap().count, 2);
    }
}
