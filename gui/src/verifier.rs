//! Checks the found files in the background, so that the results can show
//! which ones will open ("Verified") before anything is saved.
//!
//! The structure check is `wdfr::verify`; photos are also decoded, which
//! catches damage inside the picture data that leaves the structure intact.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use wdfr::recover::{self, Found, Item, Session};
use wdfr::verify::{self, Verdict};

use crate::results::RowRef;

/// Photos are decoded up to this size; larger ones get the structure check only.
const MAX_DECODE: u64 = 32 << 20;

pub struct Verifier {
    new: Arc<Mutex<Vec<(RowRef, Verdict)>>>,
    done: Arc<AtomicUsize>,
    total: usize,
    cancel: Arc<AtomicBool>,
}

impl Verifier {
    pub fn start(ctx: &egui::Context, session: Arc<Session>, found: Arc<Found>, refs: Vec<RowRef>) -> Self {
        let new = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let total = refs.len();
        let (n, d, c, ctx) = (new.clone(), done.clone(), cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut shown = Instant::now();
            for r in refs {
                if c.load(Ordering::Relaxed) {
                    return;
                }
                let verdict = check(&session, r.item(&found));
                if let Ok(mut v) = n.lock() {
                    v.push((r, verdict));
                }
                d.fetch_add(1, Ordering::Relaxed);
                if shown.elapsed() > Duration::from_millis(300) {
                    ctx.request_repaint();
                    shown = Instant::now();
                }
            }
            ctx.request_repaint();
        });
        Self { new, done, total, cancel }
    }

    /// Verdicts reached since the last call.
    pub fn take_new(&self) -> Vec<(RowRef, Verdict)> {
        self.new.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
    }

    /// (files checked, files to check).
    pub fn progress(&self) -> (usize, usize) {
        (self.done.load(Ordering::Relaxed), self.total)
    }

    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed) >= self.total
    }
}

impl Drop for Verifier {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn check(session: &Session, item: Item) -> Verdict {
    let verdict = verify::check(session, item);
    let ext = match item {
        Item::Fs(f) => f.file.name().rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default(),
        Item::Carved(c) => c.ext.to_string(),
    };
    let decodable = matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff");
    if verdict != Verdict::Verified || !decodable {
        return verdict;
    }
    match recover::read_item(session, item, MAX_DECODE) {
        Ok(Some(data)) => match image::load_from_memory(&data) {
            Ok(_) => Verdict::Verified,
            Err(_) => Verdict::Damaged,
        },
        _ => verdict,
    }
}
