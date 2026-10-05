//! Background work (scanning, saving, drive discovery) with progress that
//! the UI can read at any time.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use eframe::egui;
use wdfr::carve::Category;
use wdfr::progress::{Progress, Unit};

/// Index into [`ProgressState::by_category`]: the six categories + other.
pub fn category_slot(c: Option<Category>) -> usize {
    c.and_then(|c| Category::ALL.iter().position(|x| *x == c)).unwrap_or(Category::ALL.len())
}

#[derive(Clone)]
pub struct ProgressState {
    pub task: String,
    pub unit: Unit,
    pub total: u64,
    pub done: u64,
    pub item: String,
    pub found: u64,
    pub by_category: [u64; 7],
    pub warnings: Vec<String>,
    /// Smoothed throughput in units per second.
    pub rate: f64,
    mark: (Instant, u64),
}

impl Default for ProgressState {
    fn default() -> Self {
        Self {
            task: "Starting...".into(),
            unit: Unit::Items,
            total: 0,
            done: 0,
            item: String::new(),
            found: 0,
            by_category: [0; 7],
            warnings: Vec::new(),
            rate: 0.0,
            mark: (Instant::now(), 0),
        }
    }
}

impl ProgressState {
    fn update_rate(&mut self) {
        let elapsed = self.mark.0.elapsed().as_secs_f64();
        if elapsed >= 0.5 {
            let inst = self.done.saturating_sub(self.mark.1) as f64 / elapsed;
            self.rate = if self.rate == 0.0 { inst } else { 0.7 * self.rate + 0.3 * inst };
            self.mark = (Instant::now(), self.done);
        }
    }

    pub fn fraction(&self) -> Option<f32> {
        (self.total > 0).then(|| (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32)
    }

    pub fn eta(&self) -> Option<Duration> {
        if self.rate <= 0.0 || self.total == 0 || self.done >= self.total {
            return None;
        }
        Some(Duration::from_secs_f64((self.total - self.done) as f64 / self.rate))
    }
}

#[derive(Default)]
pub struct GuiProgress {
    state: Mutex<ProgressState>,
}

impl GuiProgress {
    pub fn snapshot(&self) -> ProgressState {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ProgressState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Progress for GuiProgress {
    fn begin(&self, task: &str, total: u64, unit: Unit) {
        let mut s = self.lock();
        s.task = task.to_string();
        s.total = total;
        s.done = 0;
        s.unit = unit;
        s.item.clear();
        s.rate = 0.0;
        s.mark = (Instant::now(), 0);
    }

    fn set(&self, done: u64, total: u64) {
        let mut s = self.lock();
        s.done = done;
        s.total = total;
        s.update_rate();
    }

    fn inc(&self, n: u64) {
        let mut s = self.lock();
        s.done += n;
        s.update_rate();
    }

    fn item(&self, name: &str) {
        name.clone_into(&mut self.lock().item);
    }

    fn found(&self, category: Option<Category>) {
        let mut s = self.lock();
        s.found += 1;
        s.by_category[category_slot(category)] += 1;
    }

    fn warn(&self, msg: &str) {
        log::warn!("{msg}");
        let mut s = self.lock();
        if s.warnings.len() < 500 {
            s.warnings.push(msg.to_string());
        }
    }

    fn error(&self, msg: &str) {
        self.warn(msg);
    }
}

/// A unit of background work producing a `T`.
pub struct Job<T> {
    pub progress: Arc<GuiProgress>,
    pub cancel: Arc<AtomicBool>,
    pub started: Instant,
    rx: Receiver<Result<T>>,
}

impl<T: Send + 'static> Job<T> {
    pub fn spawn(
        ctx: &egui::Context,
        work: impl FnOnce(&GuiProgress, &AtomicBool) -> Result<T> + Send + 'static,
    ) -> Self {
        let progress = Arc::new(GuiProgress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p, c, ctx) = (progress.clone(), cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| work(&p, &c)))
                .unwrap_or_else(|_| Err(anyhow!("an internal error occurred (please report it)")));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        Self { progress, cancel, started: Instant::now(), rx }
    }

    /// The result, once the work has finished.
    pub fn poll(&self) -> Option<Result<T>> {
        self.rx.try_recv().ok()
    }

    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn stopping(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}
