//! Recovery orchestration, in two phases:
//!
//! 1. [`scan`] finds recoverable files — first through file-system metadata,
//!    then by carving the space that is still unaccounted for — without
//!    writing anything.
//! 2. [`save`] writes a chosen subset of them, with a CSV report.
//!
//! [`run`] chains both for the command line; the desktop app lets the user
//! choose between them.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::carve::{self, CarveOptions, Carved, Category};
use crate::filter::Filter;
use crate::fs::{self, Condition, DeletedFile};
use crate::output::{self, Report, ReportRow};
use crate::partition::{self, Partition};
use crate::progress::{Progress, Unit};
use crate::ranges::{self, ByteRange};
use crate::source::{DiskSource, ReadAt, Source, read_tolerant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
pub enum Method {
    /// File-system metadata first, then carve the remaining free space.
    All,
    /// Only file-system metadata (keeps original names and folders).
    Fs,
    /// Only signature carving (works on formatted or unknown file systems).
    Carve,
}

/// What to look for, and where.
pub struct ScanOptions {
    pub method: Method,
    /// Only this partition (its `index`), or all.
    pub partition: Option<usize>,
    pub filter: Filter,
    /// Carve the whole disk instead of only unallocated space.
    pub carve_all_space: bool,
    /// Carving candidate alignment: 512 (sector) or 1 (byte).
    pub step: u64,
    pub max_carve_size: Option<u64>,
}

/// How the output folder is organised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Layout {
    /// `partition1_NTFS/<original path>` and `carved/<category>/...`.
    Original,
    /// `<category>/<file name>` for everything.
    ByType,
}

/// Where and how to write recovered files.
pub struct SaveOptions {
    pub out: PathBuf,
    pub layout: Layout,
    /// Give recovered files their original modification time.
    pub restore_dates: bool,
    pub write_report: bool,
    pub allow_same_volume: bool,
}

/// Command-line recovery: scan, then save everything that passes.
pub struct Options {
    pub scan: ScanOptions,
    pub save: SaveOptions,
    pub include_overwritten: bool,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub fs_files: u64,
    pub fs_bytes: u64,
    pub carved_files: u64,
    pub carved_bytes: u64,
    pub skipped_overwritten: u64,
    pub unreadable_bytes: u64,
    pub failures: u64,
    pub cancelled: bool,
    pub report: Option<PathBuf>,
}

pub struct Session {
    pub disk: Source,
    pub path: String,
    pub partitions: Vec<Partition>,
}

impl Session {
    pub fn open(path: &str) -> Result<Self> {
        let d = DiskSource::open(path)?;
        let path = d.path().to_string();
        let disk: Source = Arc::new(d);
        let partitions = partition::discover(&disk);
        Ok(Self { disk, path, partitions })
    }

    pub fn selected(&self, only: Option<usize>) -> Result<Vec<&Partition>> {
        let v: Vec<&Partition> = self.partitions.iter().filter(|p| only.is_none_or(|i| p.index == i)).collect();
        if v.is_empty() {
            anyhow::bail!("partition {} not found (see `wdfr info`)", only.unwrap_or(0));
        }
        Ok(v)
    }

    fn position(&self, p: &Partition) -> usize {
        self.partitions.iter().position(|q| std::ptr::eq(q, p)).unwrap_or(0)
    }
}

/// A deleted file found through file-system metadata.
#[derive(Debug, Clone)]
pub struct FsFound {
    /// Position in [`Session::partitions`].
    pub partition: usize,
    pub file: DeletedFile,
}

/// Everything a scan found.
#[derive(Debug, Default)]
pub struct Found {
    pub fs: Vec<FsFound>,
    pub carved: Vec<Carved>,
    pub cancelled: bool,
}

/// One file to save.
#[derive(Debug, Clone, Copy)]
pub enum Item<'a> {
    Fs(&'a FsFound),
    Carved(&'a Carved),
}

impl Item<'_> {
    pub fn name(&self) -> String {
        match self {
            Item::Fs(f) => f.file.name().to_string(),
            Item::Carved(c) => carved_name(c),
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            Item::Fs(f) => f.file.size,
            Item::Carved(c) => c.len,
        }
    }

    pub fn category(&self) -> Option<Category> {
        match self {
            Item::Fs(f) => file_category(&f.file),
            Item::Carved(c) => Some(c.category),
        }
    }
}

pub fn carved_name(c: &Carved) -> String {
    format!("f{:012x}.{}", c.offset, c.ext)
}

pub fn file_category(f: &DeletedFile) -> Option<Category> {
    f.name().rsplit_once('.').and_then(|(_, e)| carve::category_for_ext(e))
}

/// An opened file system and the deleted files found on it.
pub type ScanResult = (Box<dyn fs::Volume>, Vec<DeletedFile>);

/// Deleted files on one partition, filtered. `None` if the partition has
/// no supported file system.
pub fn scan_partition(
    session: &Session,
    p: &Partition,
    filter: &Filter,
    progress: &dyn Progress,
) -> Result<Option<ScanResult>> {
    if p.fs.is_none() {
        return Ok(None);
    }
    let vol = fs::open(p.source(&session.disk)).with_context(|| format!("opening {}", p.label()))?;
    progress.begin(&format!("Reading {} file system", p.label()), 0, Unit::Items);
    let files = vol.scan_deleted(&mut |done, total| progress.set(done, total));
    progress.end();
    let mut files = files?;
    files.retain(|f| filter.matches_file(f));
    Ok(Some((vol, files)))
}

/// Finds recoverable files without writing anything.
pub fn scan(session: &Session, opts: &ScanOptions, progress: &dyn Progress, cancel: &AtomicBool) -> Result<Found> {
    let mut found = Found::default();
    let mut claimed: Vec<ByteRange> = Vec::new();
    let mut free: Vec<ByteRange> = Vec::new();

    for p in session.selected(opts.partition)? {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let vol = if opts.method == Method::Carve {
            p.fs.and_then(|_| fs::open(p.source(&session.disk)).ok())
        } else {
            match scan_partition(session, p, &opts.filter, progress) {
                Ok(Some((vol, files))) => {
                    let pos = session.position(p);
                    for file in files {
                        // Space holding intact files needs no carving.
                        if file.condition == Condition::Recoverable {
                            claimed.extend(file.data_ranges().into_iter().map(|r| p.start + r.start..p.start + r.end));
                        }
                        progress.found(file_category(&file));
                        found.fs.push(FsFound { partition: pos, file });
                    }
                    Some(vol)
                }
                Ok(None) => None,
                Err(e) => {
                    progress.warn(&format!("{}: file-system scan failed: {e:#}", p.label()));
                    None
                }
            }
        };
        // Work out which part of this partition is worth carving.
        if opts.method != Method::Fs {
            let unalloc = match (&vol, opts.carve_all_space) {
                (Some(v), false) => v.free_ranges().map_err(|e| {
                    progress.warn(&format!("{}: no allocation map ({e:#}); searching the whole partition", p.label()));
                }),
                _ => Err(()),
            };
            match unalloc {
                Ok(r) => free.extend(r.into_iter().map(|r| p.start + r.start..p.start + r.end)),
                Err(()) => free.push(p.range()),
            }
        }
    }

    if opts.method != Method::Fs && !cancel.load(Ordering::Relaxed) {
        if opts.partition.is_none() {
            free.extend(partition::unpartitioned(session.disk.size(), &session.partitions));
        }
        let todo = ranges::subtract(&ranges::normalize(free), &ranges::normalize(claimed));
        found.carved = carve_ranges(session, &todo, opts, progress, cancel)?;
    }
    found.cancelled = cancel.load(Ordering::Relaxed);
    Ok(found)
}

fn carve_ranges(
    session: &Session,
    todo: &[ByteRange],
    opts: &ScanOptions,
    progress: &dyn Progress,
    cancel: &AtomicBool,
) -> Result<Vec<Carved>> {
    let formats: Vec<_> = carve::all_formats().into_iter().filter(|f| opts.filter.wants_format(*f)).collect();
    let mut out = Vec::new();
    if formats.is_empty() || todo.is_empty() {
        return Ok(out);
    }
    progress.begin("Deep search", ranges::total(todo), Unit::Bytes);
    let carve_opts = CarveOptions { formats, step: opts.step, max_size: opts.max_carve_size };
    let stats = carve::carve(
        session.disk.as_ref(),
        todo,
        &carve_opts,
        cancel,
        |c: &Carved| {
            // Files rejected by size are still skipped over by the scanner,
            // so nothing inside them is mistaken for another file.
            if opts.filter.matches_carved(c) {
                out.push(c.clone());
                progress.found(Some(c.category));
                progress.item(&format!("{} files found", out.len()));
            }
            Ok(())
        },
        |n| progress.inc(n),
    );
    progress.end();
    let stats = stats?;
    log::info!("carving scanned {} bytes, {} candidates accepted", stats.scanned, stats.found);
    Ok(out)
}

/// Writes `items` into `opts.out`.
pub fn save(
    session: &Session,
    items: &[Item],
    opts: &SaveOptions,
    progress: &dyn Progress,
    cancel: &AtomicBool,
) -> Result<Summary> {
    if !opts.allow_same_volume {
        output::ensure_not_on_source(&session.path, &opts.out)?;
    }
    std::fs::create_dir_all(&opts.out).with_context(|| format!("creating {}", opts.out.display()))?;
    let mut report = if opts.write_report { Some(Report::create(&opts.out)?) } else { None };
    let mut sum = Summary::default();
    let mut sources: Vec<Option<Source>> = vec![None; session.partitions.len()];

    progress.begin("Recovering files", items.iter().map(Item::size).sum(), Unit::Bytes);
    for item in items {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        progress.item(&item.name());
        let result = match item {
            Item::Fs(f) => {
                let p = &session.partitions[f.partition];
                let src = sources[f.partition].get_or_insert_with(|| p.source(&session.disk)).clone();
                save_fs(&src, p, f, opts).map(|(path, st)| {
                    sum.fs_files += 1;
                    sum.fs_bytes += st.written;
                    sum.unreadable_bytes += st.unreadable;
                    ReportRow {
                        method: "filesystem",
                        partition: p.label(),
                        original_path: f.file.path.clone(),
                        recovered_path: rel(&opts.out, &path),
                        size: f.file.size,
                        disk_offset: f
                            .file
                            .data_ranges()
                            .first()
                            .map(|r| format!("{:#x}", p.start + r.start))
                            .unwrap_or_default(),
                        condition: f.file.condition.to_string(),
                        modified: f.file.modified.map(|t| t.to_string()).unwrap_or_default(),
                        note: f.file.note.clone().unwrap_or_default(),
                        unreadable_bytes: st.unreadable,
                    }
                })
            }
            Item::Carved(c) => save_carved(session.disk.as_ref(), c, opts).map(|(path, unreadable)| {
                sum.carved_files += 1;
                sum.carved_bytes += c.len;
                sum.unreadable_bytes += unreadable;
                ReportRow {
                    method: "carved",
                    partition: partition_of(&session.partitions, c.offset),
                    original_path: String::new(),
                    recovered_path: rel(&opts.out, &path),
                    size: c.len,
                    disk_offset: format!("{:#x}", c.offset),
                    condition: "carved".into(),
                    modified: String::new(),
                    note: c.format.to_string(),
                    unreadable_bytes: unreadable,
                }
            }),
        };
        match result {
            Ok(row) => {
                if let Some(r) = report.as_mut() {
                    r.add(&row)?;
                }
            }
            Err(e) => {
                sum.failures += 1;
                progress.error(&format!("{}: {e:#}", item.name()));
            }
        }
        progress.inc(item.size());
    }
    progress.end();
    sum.cancelled = cancel.load(Ordering::Relaxed);
    sum.report = report.map(Report::finish).transpose()?;
    Ok(sum)
}

/// Scan + save everything that passes the filters (the command line).
pub fn run(session: &Session, opts: &Options, progress: &dyn Progress, cancel: &AtomicBool) -> Result<Summary> {
    // Fail before a possibly hours-long scan, not after it.
    if !opts.save.allow_same_volume {
        output::ensure_not_on_source(&session.path, &opts.save.out)?;
    }
    let found = scan(session, &opts.scan, progress, cancel)?;
    let mut skipped = 0;
    let mut items: Vec<Item> = Vec::with_capacity(found.fs.len() + found.carved.len());
    for f in &found.fs {
        if opts.include_overwritten || f.file.condition != Condition::Overwritten {
            items.push(Item::Fs(f));
        } else {
            skipped += 1;
        }
    }
    items.extend(found.carved.iter().map(Item::Carved));
    // A scan stopped early still saves what it found.
    let never = AtomicBool::new(false);
    let save_cancel = if found.cancelled { &never } else { cancel };
    let mut sum = save(session, &items, &opts.save, progress, save_cancel)?;
    sum.skipped_overwritten = skipped;
    sum.cancelled |= found.cancelled;
    Ok(sum)
}

/// Reads the content of an item into memory (for previews). Returns
/// `None` if it is larger than `limit`.
pub fn read_item(session: &Session, item: Item, limit: u64) -> Result<Option<Vec<u8>>> {
    if item.size() > limit {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(item.size() as usize);
    match item {
        Item::Fs(f) => {
            let src = session.partitions[f.partition].source(&session.disk);
            fs::extract(src.as_ref(), &f.file, &mut out)?;
        }
        Item::Carved(c) => {
            copy_range(session.disk.as_ref(), c.offset, c.len, &mut out)?;
        }
    }
    Ok(Some(out))
}

fn category_dir(c: Option<Category>) -> &'static str {
    c.map_or("other", Category::dir_name)
}

fn save_fs(vol: &Source, p: &Partition, f: &FsFound, opts: &SaveOptions) -> Result<(PathBuf, fs::ExtractStats)> {
    let file = &f.file;
    let (target, fallback_dir) = match opts.layout {
        Layout::Original => {
            let base = opts.out.join(p.label());
            (base.join(output::safe_relative_path(&file.path)), base.join("_flat"))
        }
        Layout::ByType => {
            let dir = opts.out.join(category_dir(file_category(file)));
            (dir.join(output::sanitize_component(file.name())), dir)
        }
    };
    // Deep or exotic paths can still fail on some systems; fall back to a
    // flat name rather than losing the file.
    let (path, out) = output::create_unique(target).or_else(|_| {
        output::create_unique(fallback_dir.join(format!("{}_{}", file.id, output::sanitize_component(file.name()))))
    })?;
    let mut w = BufWriter::with_capacity(1 << 20, out);
    let st = fs::extract(vol.as_ref(), file, &mut w).with_context(|| format!("writing {}", path.display()))?;
    let out = w.into_inner().map_err(|e| e.into_error())?;
    if opts.restore_dates
        && let Some(m) = file.modified
    {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(m.and_utc().timestamp().max(0) as u64);
        let _ = out.set_modified(t);
    }
    Ok((path, st))
}

fn save_carved(disk: &dyn ReadAt, c: &Carved, opts: &SaveOptions) -> Result<(PathBuf, u64)> {
    let dir = match opts.layout {
        Layout::Original => opts.out.join("carved").join(c.category.dir_name()),
        Layout::ByType => opts.out.join(c.category.dir_name()),
    };
    let (path, file) = output::create_unique(dir.join(carved_name(c)))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    let unreadable = copy_range(disk, c.offset, c.len, &mut w)?;
    w.flush()?;
    Ok((path, unreadable))
}

fn copy_range(src: &dyn ReadAt, offset: u64, len: u64, out: &mut dyn Write) -> Result<u64> {
    let mut buf = vec![0u8; (1u64 << 20).min(len.max(1)) as usize];
    let (mut pos, mut bad) = (0u64, 0u64);
    while pos < len {
        let n = (len - pos).min(buf.len() as u64) as usize;
        bad += read_tolerant(src, offset + pos, &mut buf[..n]);
        out.write_all(&buf[..n])?;
        pos += n as u64;
    }
    Ok(bad)
}

fn partition_of(parts: &[Partition], offset: u64) -> String {
    parts.iter().find(|p| p.range().contains(&offset)).map_or_else(|| "unpartitioned".into(), Partition::label)
}

fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base).unwrap_or(p).to_string_lossy().replace('\\', "/")
}
