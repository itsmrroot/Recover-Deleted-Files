//! The carving loop: scans byte ranges for signatures and measures hits.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use chrono::NaiveDateTime;

use super::{Category, Format, HEAD_LEN, Reader};
use crate::ranges::ByteRange;
use crate::source::{ReadAt, read_tolerant};

const BLOCK: usize = 8 << 20;

pub struct CarveOptions {
    /// Formats to look for.
    pub formats: Vec<&'static dyn Format>,
    /// Candidate alignment. Files start on cluster boundaries, so 512 finds
    /// everything a file system could have stored; 1 also finds files
    /// embedded inside other data (much slower).
    pub step: u64,
    /// Overrides every format's maximum size if set.
    pub max_size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Carved {
    /// Absolute offset in the source.
    pub offset: u64,
    pub len: u64,
    pub ext: &'static str,
    pub category: Category,
    pub format: &'static str,
    /// A name read from the file's own metadata (see [`super::meta`]).
    pub title: Option<String>,
    /// When the content was made, from its metadata.
    pub date: Option<NaiveDateTime>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CarveStats {
    pub scanned: u64,
    pub found: u64,
}

/// Scans `ranges` of `src`, calling `on_hit` for every accepted file and
/// `progress(bytes_scanned)` as it goes. Stops early when `cancel` is set.
pub fn carve(
    src: &dyn ReadAt,
    ranges: &[ByteRange],
    opts: &CarveOptions,
    cancel: &AtomicBool,
    mut on_hit: impl FnMut(&Carved) -> Result<()>,
    mut progress: impl FnMut(u64),
) -> Result<CarveStats> {
    let step = opts.step.max(1);
    let mut stats = CarveStats::default();
    let mut block = vec![0u8; BLOCK + HEAD_LEN];

    // Index formats by the byte they can start with to keep the inner loop
    // tight: most offsets match no format at all.
    let mut by_first: Vec<Vec<&'static dyn Format>> = vec![Vec::new(); 256];
    for f in &opts.formats {
        let first = f.first_bytes();
        for b in 0..=255u8 {
            if first.is_empty() || first.contains(&b) {
                by_first[b as usize].push(*f);
            }
        }
    }

    for range in ranges {
        let mut pos = range.start.div_ceil(step) * step;
        'block: while pos < range.end {
            if cancel.load(Ordering::Relaxed) {
                return Ok(stats);
            }
            let len = ((range.end - pos) as usize).min(BLOCK);
            let with_head = ((range.end - pos) as usize).min(BLOCK + HEAD_LEN);
            read_tolerant(src, pos, &mut block[..with_head]);
            let mut off = 0usize;
            while off < len {
                let cands = &by_first[block[off] as usize];
                if !cands.is_empty() {
                    let head = &block[off..(off + HEAD_LEN).min(with_head)];
                    let abs = pos + off as u64;
                    for f in cands {
                        if !f.probe(head) {
                            continue;
                        }
                        let max = opts.max_size.unwrap_or_else(|| f.max_size()).min(range.end - abs);
                        let mut r = Reader::new(src, abs, max);
                        let Some(hit) = f.measure(&mut r) else { continue };
                        if hit.len == 0 || hit.len > max {
                            continue;
                        }
                        let meta = super::meta::read(f.name(), hit.ext, &mut Reader::new(src, abs, hit.len), hit.len);
                        let c = Carved {
                            offset: abs,
                            len: hit.len,
                            ext: hit.ext,
                            category: f.category_of(hit.ext),
                            format: f.name(),
                            title: meta.title,
                            date: meta.date,
                        };
                        on_hit(&c)?;
                        stats.found += 1;
                        // Resume after the file: its interior can't start
                        // another file (embedded thumbnails etc. excluded).
                        let next = (abs + hit.len).div_ceil(step) * step;
                        stats.scanned += next - pos;
                        progress(next - pos);
                        pos = next;
                        continue 'block;
                    }
                }
                off += step as usize;
            }
            stats.scanned += len as u64;
            progress(len as u64);
            pos += len as u64;
            // Keep the candidate grid aligned when a block is not a
            // multiple of the step (only possible with exotic steps).
            pos = pos.div_ceil(step) * step;
        }
    }
    Ok(stats)
}
