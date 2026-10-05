//! Previews of found files: decoded off the UI thread and cached.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui;
use wdfr::recover::{self, Found, Session};

use crate::i18n::{tr, trf};
use crate::results::RowRef;

/// Files larger than this are not read for a preview.
const MAX_PREVIEW_BYTES: u64 = 64 << 20;
const CACHE: usize = 48;

pub enum Preview {
    Loading,
    Image { texture: egui::TextureHandle, size: [u32; 2] },
    Text(String),
    Unavailable(String),
}

enum Decoded {
    Image(egui::ColorImage, [u32; 2]),
    Text(String),
    Unavailable(String),
}

#[derive(Default)]
pub struct Previewer {
    cache: HashMap<RowRef, Preview>,
    order: VecDeque<RowRef>,
    pending: HashMap<RowRef, Receiver<Decoded>>,
}

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff"];
const TEXT_EXTS: &[&str] = &["txt", "csv", "json", "xml", "html", "htm", "md", "log", "ini", "cfg", "yml", "yaml"];

impl Previewer {
    pub fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
        self.pending.clear();
    }

    pub fn get(&mut self, session: &Arc<Session>, found: &Arc<Found>, r: RowRef, ext: &str) -> &Preview {
        if !self.cache.contains_key(&r) {
            let ext = ext.to_ascii_lowercase();
            let kind = if IMAGE_EXTS.contains(&ext.as_str()) {
                Some(true)
            } else if TEXT_EXTS.contains(&ext.as_str()) {
                Some(false)
            } else {
                None
            };
            let entry = match kind {
                None => Preview::Unavailable(tr("No preview for this file type.").into()),
                Some(is_image) => {
                    let (tx, rx) = channel();
                    let (session, found) = (session.clone(), found.clone());
                    std::thread::spawn(move || {
                        let _ = tx.send(decode(&session, &found, r, is_image));
                    });
                    self.pending.insert(r, rx);
                    Preview::Loading
                }
            };
            self.insert(r, entry);
        }
        &self.cache[&r]
    }

    /// Moves finished decodes into the cache. Returns true if any arrived.
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        let done: Vec<(RowRef, Decoded)> =
            self.pending.iter().filter_map(|(r, rx)| rx.try_recv().ok().map(|d| (*r, d))).collect();
        let any = !done.is_empty();
        for (r, d) in done {
            self.pending.remove(&r);
            let p = match d {
                Decoded::Image(img, size) => Preview::Image {
                    texture: ctx.load_texture(format!("preview-{r:?}"), img, egui::TextureOptions::LINEAR),
                    size,
                },
                Decoded::Text(t) => Preview::Text(t),
                Decoded::Unavailable(why) => Preview::Unavailable(why),
            };
            self.cache.insert(r, p);
        }
        any
    }

    pub fn loading(&self) -> bool {
        !self.pending.is_empty()
    }

    fn insert(&mut self, r: RowRef, p: Preview) {
        self.cache.insert(r, p);
        self.order.push_back(r);
        while self.order.len() > CACHE {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
                self.pending.remove(&old);
            }
        }
    }
}

fn decode(session: &Session, found: &Found, r: RowRef, is_image: bool) -> Decoded {
    let item = r.item(found);
    let bytes = match recover::read_item(session, item, MAX_PREVIEW_BYTES) {
        Ok(Some(b)) => b,
        Ok(None) => return Decoded::Unavailable(tr("Too large to preview.").into()),
        Err(e) => return Decoded::Unavailable(trf("Could not read the file: {error}", &[("error", &e)])),
    };
    if !is_image {
        let head = &bytes[..bytes.len().min(8000)];
        if head.iter().filter(|&&b| b == 0).count() > head.len() / 50 {
            return Decoded::Unavailable(tr("The content does not look like text (it may be overwritten).").into());
        }
        return Decoded::Text(String::from_utf8_lossy(head).into_owned());
    }
    match image::load_from_memory(&bytes) {
        Ok(img) => {
            let size = [img.width(), img.height()];
            let thumb = img.thumbnail(720, 720).to_rgba8();
            let (w, h) = (thumb.width() as usize, thumb.height() as usize);
            Decoded::Image(egui::ColorImage::from_rgba_unmultiplied([w, h], thumb.as_raw()), size)
        }
        Err(_) => {
            Decoded::Unavailable(tr("The image could not be decoded. It may be damaged or partly overwritten.").into())
        }
    }
}
