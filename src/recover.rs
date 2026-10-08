//! Recovery orchestration, in two phases:
//!
//! 1. [`scan`] finds recoverable files — first through file-system metadata,
//!    then by carving the space that is still unaccounted for — without
//!    writing anything.
//! 2. [`save`] writes a chosen subset of them, with a CSV report.
//!
//! [`run`] chains both for the command line; the desktop app lets the user
//! choose between them.

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
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
use crate::{fragments, rescue};

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
    /// Save everything into one ZIP protected with this password (AES-256)
    /// instead of a folder: nothing is written unencrypted.
    pub password: Option<String>,
}

/// Command-line recovery: scan, then save everything that passes.
pub struct Options {
    pub scan: ScanOptions,
    pub save: SaveOptions,
    pub include_overwritten: bool,
    /// Also write files that are identical to another recovered file.
    pub keep_duplicates: bool,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub fs_files: u64,
    pub fs_bytes: u64,
    pub carved_files: u64,
    pub carved_bytes: u64,
    pub skipped_overwritten: u64,
    pub skipped_duplicates: u64,
    pub unreadable_bytes: u64,
    pub failures: u64,
    pub cancelled: bool,
    pub report: Option<PathBuf>,
    /// The password-protected ZIP everything went into, if one was asked for.
    pub archive: Option<PathBuf>,
}

pub struct Session {
    pub disk: Source,
    pub path: String,
    /// The partitions in the partition table (or the volume itself).
    pub partitions: Vec<Partition>,
    /// Partitions and old file tables found by a deep search (see
    /// [`crate::rescue`]). They are numbered after `partitions`.
    found: RwLock<Vec<Partition>>,
}

impl Session {
    pub fn open(path: &str) -> Result<Self> {
        let d = DiskSource::open(path)?;
        let path = d.path().to_string();
        let disk: Source = Arc::new(d);
        let partitions = partition::discover(&disk);
        Ok(Self::new(disk, path, partitions))
    }

    pub fn new(disk: Source, path: String, partitions: Vec<Partition>) -> Self {
        let disk = crate::bitlocker::overlay(disk, &partitions);
        Self { disk, path, partitions, found: RwLock::new(Vec::new()) }
    }

    /// Partition number `i`, from the table or found by a deep search.
    pub fn partition(&self, i: usize) -> Option<Partition> {
        match self.partitions.get(i) {
            Some(p) => Some(p.clone()),
            None => self.found.read().ok()?.get(i - self.partitions.len()).cloned(),
        }
    }

    /// Partitions found by a deep search so far.
    pub fn found_partitions(&self) -> Vec<Partition> {
        self.found.read().map(|f| f.clone()).unwrap_or_default()
    }

    /// Adds a partition found by a deep search and returns its number.
    pub fn add_found(&self, p: Partition) -> usize {
        let mut found = self.found.write().unwrap_or_else(|e| e.into_inner());
        found.push(p);
        self.partitions.len() + found.len() - 1
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
    /// Deleted files were erased by the drive itself (SSD TRIM).
    pub erased_by_drive: bool,
    /// Every duplicate, mapped to the copy it is identical to (see
    /// [`crate::dedupe`]).
    pub duplicates: HashMap<ItemRef, ItemRef>,
}

/// Identifies a found file: an index into [`Found::fs`] or [`Found::carved`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum ItemRef {
    Fs(usize),
    Carved(usize),
}

impl ItemRef {
    pub fn item(self, found: &Found) -> Item<'_> {
        match self {
            ItemRef::Fs(i) => Item::Fs(&found.fs[i]),
            ItemRef::Carved(i) => Item::Carved(&found.carved[i]),
        }
    }
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

/// A carved file's name: from its own metadata when it has some
/// ("2023-07-14 15.32.10 Canon EOS 80D.jpg"), otherwise from where it was
/// found ("f0001a2b3000.jpg").
pub fn carved_name(c: &Carved) -> String {
    match &c.title {
        Some(t) => format!("{t}.{}", c.ext),
        None => format!("f{:012x}.{}", c.offset, c.ext),
    }
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
    // Before filtering, so that name filters see the original names.
    fs::recycle::restore_names(&mut files, |f| {
        let mut out = Vec::new();
        fs::extract(vol.source().as_ref(), f, &mut out).ok()?;
        Some(out)
    });
    if mark_erased(vol.source().as_ref(), &mut files) {
        progress.warn(&format!(
            "{}: the drive has erased the data of deleted files itself (SSD TRIM); they contain only zeros",
            p.label()
        ));
    }
    files.retain(|f| filter.matches_file(f));
    Ok(Some((vol, files)))
}

/// Bytes read at each place [`mark_erased`] looks at.
const PROBE: usize = 4096;
/// Files checked before deciding whether the drive erases deleted data.
const ERASE_SAMPLE: usize = 64;

/// Detects deleted files whose data reads back as zeros at its start,
/// middle and end: an SSD with TRIM erases freed space, so the file system
/// still says "free" but nothing is left. To keep hard drives fast, a
/// sample is checked first, and every file only when the sample shows that
/// the drive erases deleted data. Returns true if it does.
fn mark_erased(vol: &dyn ReadAt, files: &mut [DeletedFile]) -> bool {
    let candidates: Vec<usize> = (0..files.len())
        .filter(|&i| {
            matches!(files[i].condition, Condition::Recoverable | Condition::Partial(_))
                && matches!(files[i].data, fs::FileData::Extents(_))
                && files[i].size >= PROBE as u64
        })
        .collect();
    if candidates.is_empty() {
        return false;
    }
    let step = candidates.len().div_ceil(ERASE_SAMPLE);
    let sample: Vec<usize> = candidates.iter().copied().step_by(step).collect();
    let erased = sample.iter().filter(|&&i| reads_as_zeros(vol, &files[i])).count();
    // TRIM erases (nearly) everything; a few all-zero files are just files.
    if erased * 2 < sample.len() {
        return false;
    }
    for i in candidates {
        if reads_as_zeros(vol, &files[i]) {
            files[i].condition = Condition::Erased;
        }
    }
    true
}

/// True when the file's data is zero at its start, middle and end.
fn reads_as_zeros(vol: &dyn ReadAt, f: &DeletedFile) -> bool {
    let fs::FileData::Extents(extents) = &f.data else { return false };
    let size = f.size;
    let probes = [0, (size / 2).saturating_sub(PROBE as u64 / 2), size.saturating_sub(PROBE as u64)];
    let mut buf = vec![0u8; PROBE];
    let mut checked = 0;
    for logical in probes {
        // Map the logical offset to a position on the volume.
        let mut start = 0u64;
        let mut physical = None;
        for e in extents {
            if logical < start + e.len {
                physical = Some(e.offset.map(|o| o + (logical - start)));
                break;
            }
            start += e.len;
        }
        match physical {
            Some(Some(p)) => {
                let n = ((size - logical) as usize).min(PROBE);
                read_tolerant(vol, p, &mut buf[..n]);
                if buf[..n].iter().any(|&b| b != 0) {
                    return false;
                }
                checked += 1;
            }
            // A sparse run is zeros by design, not an erased file.
            Some(None) => return false,
            None => {}
        }
    }
    checked > 0
}

/// Finds recoverable files without writing anything.
pub fn scan(session: &Session, opts: &ScanOptions, progress: &dyn Progress, cancel: &AtomicBool) -> Result<Found> {
    let mut found = Found::default();
    let mut claimed: Vec<ByteRange> = Vec::new();
    let mut free: Vec<ByteRange> = Vec::new();
    // Cluster size of each volume, to put fragmented videos back together.
    let mut clusters: Vec<(ByteRange, u64)> = Vec::new();

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
                        found.erased_by_drive |= file.condition == Condition::Erased;
                        let f = FsFound { partition: pos, file };
                        progress.file_found(&f);
                        found.fs.push(f);
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
        if let Some(v) = &vol {
            clusters.push((p.range(), v.cluster_size()));
        }
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

    if opts.method != Method::Carve && !cancel.load(Ordering::Relaxed) {
        previous_versions(session, opts, progress, cancel, &mut found);
    }

    if opts.method != Method::Fs && !cancel.load(Ordering::Relaxed) {
        if opts.partition.is_none() {
            free.extend(partition::unpartitioned(session.disk.size(), &session.partitions));
        }
        let free = ranges::normalize(free);
        let todo = ranges::subtract(&free, &ranges::normalize(claimed));
        // Lost partitions and old file tables live in the same space.
        let mut finder = rescue::Finder::new();
        let mut videos = fragments::VideoFinder::new();
        found.carved = carve_ranges(session, &todo, opts, progress, cancel, Some(&mut finder), &mut videos)?;
        if !cancel.load(Ordering::Relaxed) {
            progress.begin("Rebuilding lost partitions", 0, Unit::Items);
            for r in finder.finish(session, &free) {
                let pos = session.add_found(r.partition);
                for file in r.files.into_iter().filter(|f| opts.filter.matches_file(f)) {
                    progress.found(file_category(&file));
                    let f = FsFound { partition: pos, file };
                    progress.file_found(&f);
                    found.fs.push(f);
                }
            }
            progress.end();
        }
        if !cancel.load(Ordering::Relaxed) {
            progress.begin("Rebuilding fragmented videos", 0, Unit::Items);
            // Without a file system, 4 KiB: any cluster size is a multiple.
            let cluster_at =
                |o: u64| clusters.iter().find(|(r, _)| r.contains(&o)).map_or(4096, |c| c.1).max(SECTOR_SIZE);
            let rebuilt = videos.rebuild(session.disk.as_ref(), &mut found.carved, &cluster_at, cancel);
            if !rebuilt.is_empty() {
                let pos = session.add_found(Partition {
                    index: session.found_partitions().len() + 1,
                    start: 0,
                    len: session.disk.size(),
                    scheme: partition::Scheme::Found,
                    kind: "rebuilt videos".into(),
                    name: String::new(),
                    fs: None,
                    device: None,
                });
                for v in rebuilt {
                    let file = rebuilt_file(v);
                    if opts.filter.matches_file(&file) {
                        progress.found(file_category(&file));
                        let f = FsFound { partition: pos, file };
                        progress.file_found(&f);
                        found.fs.push(f);
                    }
                }
            }
            progress.end();
        }
    }
    if !cancel.load(Ordering::Relaxed) {
        found.duplicates = crate::dedupe::find(session, &found, progress, cancel);
    }
    found.cancelled = cancel.load(Ordering::Relaxed);
    Ok(found)
}

/// Shadow copies read at most per scan (newest first).
const MAX_SHADOW_COPIES: usize = 8;

/// Files deleted since a Windows shadow copy ("Previous Versions") of the
/// drive was taken: complete in the copy, whatever happened on the drive.
fn previous_versions(
    session: &Session,
    opts: &ScanOptions,
    progress: &dyn Progress,
    cancel: &AtomicBool,
    found: &mut Found,
) {
    let copies = crate::shadow::list(&session.path);
    if copies.is_empty() {
        return;
    }
    // What the drive has now.
    let Some(current) = session.partitions.first().and_then(|p| fs::open(p.source(&session.disk)).ok()) else {
        return;
    };
    let existing: std::collections::HashSet<String> = current
        .scan_files(true, &mut |_, _| {})
        .map(|files| files.into_iter().map(|f| f.path.to_lowercase()).collect())
        .unwrap_or_default();
    progress.begin("Reading previous versions", copies.len().min(MAX_SHADOW_COPIES) as u64, Unit::Items);
    let mut seen = std::collections::HashSet::new();
    for (k, copy) in copies.iter().take(MAX_SHADOW_COPIES).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(src) = DiskSource::open(&copy.device) else { continue };
        let Ok(vol) = fs::open(Arc::new(src)) else { continue };
        let Ok(files) = vol.scan_files(true, &mut |_, _| {}) else { continue };
        let date = copy.created.map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
        let mut pos = None;
        for mut file in files {
            let key = file.path.to_lowercase();
            if system_path(&key) || existing.contains(&key) || !opts.filter.matches_file(&file) || !seen.insert(key) {
                continue;
            }
            let pos = *pos.get_or_insert_with(|| {
                session.add_found(Partition {
                    index: session.found_partitions().len() + 1,
                    start: 0,
                    len: vol.source().size(),
                    scheme: partition::Scheme::Found,
                    kind: format!("previous version {date}"),
                    name: String::new(),
                    fs: Some(vol.kind()),
                    device: Some(copy.device.clone()),
                })
            });
            file.note = Some(format!("from the Previous Versions copy of {date}"));
            progress.found(file_category(&file));
            let f = FsFound { partition: pos, file };
            progress.file_found(&f);
            found.fs.push(f);
        }
        progress.set(k as u64 + 1, copies.len().min(MAX_SHADOW_COPIES) as u64);
    }
    progress.end();
}

/// Windows' own files, which come and go all the time: not worth listing
/// from a shadow copy.
fn system_path(lower: &str) -> bool {
    let top = lower.split('/').next().unwrap_or("");
    matches!(top, "windows" | "program files" | "program files (x86)" | "programdata" | "system volume information")
        || lower.split('/').any(|c| c.starts_with('$'))
        || lower.contains("/appdata/local/temp/")
        || lower.contains("/appdata/local/microsoft/")
}

/// A rebuilt video as a found file (its pieces are its extents on the disk).
fn rebuilt_file(v: fragments::Rebuilt) -> DeletedFile {
    let pieces = v.extents.len();
    let note = if v.complete {
        format!("rebuilt from {pieces} pieces")
    } else {
        format!("rebuilt from {pieces} pieces; the rest was not found")
    };
    DeletedFile {
        id: v.start,
        path: format!("f{:012x}.{}", v.start, v.ext),
        size: v.size,
        created: v.created,
        modified: v.created,
        condition: if v.complete { Condition::Recoverable } else { Condition::Partial(v.percent.min(99)) },
        note: Some(note),
        data: fs::FileData::Extents(v.extents),
    }
}

const SECTOR_SIZE: u64 = 512;

fn carve_ranges(
    session: &Session,
    todo: &[ByteRange],
    opts: &ScanOptions,
    progress: &dyn Progress,
    cancel: &AtomicBool,
    mut finder: Option<&mut rescue::Finder>,
    videos: &mut fragments::VideoFinder,
) -> Result<Vec<Carved>> {
    let formats: Vec<_> = carve::all_formats().into_iter().filter(|f| opts.filter.wants_format(*f)).collect();
    let mut out = Vec::new();
    if (formats.is_empty() && finder.is_none()) || todo.is_empty() {
        return Ok(out);
    }
    progress.begin("Deep search", ranges::total(todo), Unit::Bytes);
    let carve_opts = CarveOptions { formats, step: opts.step, max_size: opts.max_carve_size };
    let disk = session.disk.as_ref();
    let stats = carve::carve_with_blocks(
        disk,
        todo,
        &carve_opts,
        cancel,
        |c: &Carved| {
            // Files rejected by size are still skipped over by the scanner,
            // so nothing inside them is mistaken for another file.
            if opts.filter.matches_carved(c) {
                progress.carved_found(c);
                out.push(c.clone());
                progress.found(Some(c.category));
                progress.item(&format!("{} files found", out.len()));
            }
            Ok(())
        },
        |n| progress.inc(n),
        |pos, block| {
            if let Some(f) = finder.as_deref_mut() {
                f.look(disk, pos, block);
            }
            videos.look(disk, pos, block);
        },
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
    let mut sum = Summary::default();
    let mut dest = match &opts.password {
        Some(password) => {
            let path = output::unique_path(archive_path(&opts.out));
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            let file = std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
            sum.archive = Some(path);
            Dest::Zip {
                zip: Box::new(zip::ZipWriter::new(BufWriter::with_capacity(1 << 20, file))),
                password,
                names: std::collections::HashSet::new(),
            }
        }
        None => {
            std::fs::create_dir_all(&opts.out).with_context(|| format!("creating {}", opts.out.display()))?;
            Dest::Folder
        }
    };
    let mut report = match (opts.write_report, &dest) {
        (false, _) => None,
        (true, Dest::Folder) => Some(Report::create(&opts.out)?),
        (true, Dest::Zip { .. }) => Some(Report::in_memory()),
    };
    let mut sources: HashMap<usize, (Partition, Source)> = HashMap::new();

    progress.begin("Recovering files", items.iter().map(Item::size).sum(), Unit::Bytes);
    for item in items {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        progress.item(&item.name());
        let result = match item {
            Item::Fs(f) => {
                let Some((p, src)) = sources.get(&f.partition).cloned().or_else(|| {
                    let p = session.partition(f.partition)?;
                    let src = p.source(&session.disk);
                    sources.insert(f.partition, (p.clone(), src.clone()));
                    Some((p, src))
                }) else {
                    sum.failures += 1;
                    continue;
                };
                save_fs(&src, &p, f, opts, &mut dest).map(|(path, st)| {
                    sum.fs_files += 1;
                    sum.fs_bytes += st.written;
                    sum.unreadable_bytes += st.unreadable;
                    ReportRow {
                        method: "filesystem",
                        partition: p.label(),
                        original_path: f.file.path.clone(),
                        recovered_path: path,
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
            Item::Carved(c) => save_carved(session.disk.as_ref(), c, opts, &mut dest).map(|(path, unreadable)| {
                sum.carved_files += 1;
                sum.carved_bytes += c.len;
                sum.unreadable_bytes += unreadable;
                ReportRow {
                    method: "carved",
                    partition: partition_of(&session.partitions, c.offset),
                    original_path: String::new(),
                    recovered_path: path,
                    size: c.len,
                    disk_offset: format!("{:#x}", c.offset),
                    condition: "carved".into(),
                    modified: c.date.map(|t| t.to_string()).unwrap_or_default(),
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
    match dest {
        Dest::Folder => sum.report = report.map(|r| r.finish(None)).transpose()?.flatten(),
        Dest::Zip { mut zip, password, mut names } => {
            if let Some(r) = report {
                // The report goes inside, encrypted like the files.
                let name = unique_entry(&mut names, PathBuf::from("report.csv"));
                zip.start_file(name, entry_options(password, 0, None))?;
                r.finish(Some(&mut zip))?;
            }
            zip.finish()?.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        }
    }
    Ok(sum)
}

/// Scan + save everything that passes the filters (the command line).
pub fn run(session: &Session, opts: &Options, progress: &dyn Progress, cancel: &AtomicBool) -> Result<Summary> {
    // Fail before a possibly hours-long scan, not after it.
    if !opts.save.allow_same_volume {
        output::ensure_not_on_source(&session.path, &opts.save.out)?;
    }
    let found = scan(session, &opts.scan, progress, cancel)?;
    let (mut skipped, mut skipped_dups) = (0, 0);
    let mut items: Vec<Item> = Vec::with_capacity(found.fs.len() + found.carved.len());
    let refs = (0..found.fs.len()).map(ItemRef::Fs).chain((0..found.carved.len()).map(ItemRef::Carved));
    for r in refs {
        if !opts.keep_duplicates && found.duplicates.contains_key(&r) {
            skipped_dups += 1;
            continue;
        }
        match r.item(&found) {
            Item::Fs(f) if !opts.include_overwritten && !f.file.condition.is_recoverable() => skipped += 1,
            item => items.push(item),
        }
    }
    // A scan stopped early still saves what it found.
    let never = AtomicBool::new(false);
    let save_cancel = if found.cancelled { &never } else { cancel };
    let mut sum = save(session, &items, &opts.save, progress, save_cancel)?;
    sum.skipped_overwritten = skipped;
    sum.skipped_duplicates = skipped_dups;
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
            let p = session.partition(f.partition).context("unknown partition")?;
            fs::extract(p.source(&session.disk).as_ref(), &f.file, &mut out)?;
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

/// Where recovered files are written.
enum Dest<'a> {
    /// Files in the output folder.
    Folder,
    /// One password-protected ZIP (AES-256): nothing is written unencrypted.
    Zip {
        zip: Box<zip::ZipWriter<BufWriter<std::fs::File>>>,
        password: &'a str,
        names: std::collections::HashSet<String>,
    },
}

impl Dest<'_> {
    /// Writes one file at `target` (relative to the output folder; `fallback`
    /// if that path cannot be created), its content written by `fill`.
    /// Returns where it went (relative) and what `fill` returned.
    fn write<T>(
        &mut self,
        opts: &SaveOptions,
        target: PathBuf,
        fallback: PathBuf,
        modified: Option<chrono::NaiveDateTime>,
        size: u64,
        fill: impl FnOnce(&mut dyn Write) -> Result<T>,
    ) -> Result<(String, T)> {
        let modified = modified.filter(|_| opts.restore_dates);
        match self {
            Dest::Folder => {
                // Deep or exotic paths can still fail on some systems; fall
                // back to a flat name rather than losing the file.
                let (path, out) = output::create_unique(opts.out.join(&target))
                    .or_else(|_| output::create_unique(opts.out.join(fallback)))?;
                let mut w = BufWriter::with_capacity(1 << 20, out);
                let t = fill(&mut w).with_context(|| format!("writing {}", path.display()))?;
                let out = w.into_inner().map_err(|e| e.into_error())?;
                if let Some(m) = modified {
                    set_modified(&out, m);
                }
                Ok((rel(&opts.out, &path), t))
            }
            Dest::Zip { zip, password, names } => {
                let name = unique_entry(names, target);
                zip.start_file(name.clone(), entry_options(password, size, modified))?;
                let t = fill(zip).with_context(|| format!("writing {name}"))?;
                Ok((name, t))
            }
        }
    }
}

/// The ZIP that a password-protected save writes, next to `out`.
pub fn archive_path(out: &Path) -> PathBuf {
    let mut name = out.as_os_str().to_owned();
    name.push(".zip");
    PathBuf::from(name)
}

/// A name for `path` inside the ZIP, made unique ("name (1).ext").
fn unique_entry(names: &mut std::collections::HashSet<String>, path: PathBuf) -> String {
    let joined = path.iter().map(|c| c.to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
    let (stem, ext) = match joined.rsplit_once('.') {
        Some((s, e)) if !e.contains('/') => (s.to_string(), format!(".{e}")),
        _ => (joined.clone(), String::new()),
    };
    let name = std::iter::once(joined)
        .chain((1..).map(|i| format!("{stem} ({i}){ext}")))
        .find(|n| !names.contains(&n.to_lowercase()))
        .unwrap_or_default();
    names.insert(name.to_lowercase());
    name
}

fn entry_options(
    password: &str,
    size: u64,
    modified: Option<chrono::NaiveDateTime>,
) -> zip::write::FileOptions<'_, ()> {
    use chrono::{Datelike, Timelike};
    let mut o = zip::write::FileOptions::default()
        // Photos and videos do not compress; storing keeps it fast.
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(size >= u64::from(u32::MAX))
        .with_aes_encryption(zip::AesMode::Aes256, password);
    if let Some(t) = modified
        && let Ok(dt) = zip::DateTime::from_date_and_time(
            u16::try_from(t.year()).unwrap_or(1980),
            t.month() as u8,
            t.day() as u8,
            t.hour() as u8,
            t.minute() as u8,
            t.second() as u8,
        )
    {
        o = o.last_modified_time(dt);
    }
    o
}

fn save_fs(
    vol: &Source,
    p: &Partition,
    f: &FsFound,
    opts: &SaveOptions,
    dest: &mut Dest,
) -> Result<(String, fs::ExtractStats)> {
    let file = &f.file;
    let (target, fallback_dir) = match opts.layout {
        Layout::Original => {
            let base = PathBuf::from(p.label());
            (base.join(output::safe_relative_path(&file.path)), base.join("_flat"))
        }
        Layout::ByType => {
            let dir = PathBuf::from(category_dir(file_category(file)));
            (dir.join(output::sanitize_component(file.name())), dir)
        }
    };
    let fallback = fallback_dir.join(format!("{}_{}", file.id, output::sanitize_component(file.name())));
    dest.write(opts, target, fallback, file.modified, file.size, |w| Ok(fs::extract(vol.as_ref(), file, w)?))
}

fn set_modified(file: &std::fs::File, t: chrono::NaiveDateTime) {
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(t.and_utc().timestamp().max(0) as u64);
    let _ = file.set_modified(t);
}

fn save_carved(disk: &dyn ReadAt, c: &Carved, opts: &SaveOptions, dest: &mut Dest) -> Result<(String, u64)> {
    let dir = match opts.layout {
        Layout::Original => PathBuf::from("carved").join(c.category.dir_name()),
        Layout::ByType => PathBuf::from(c.category.dir_name()),
    };
    let target = dir.join(carved_name(c));
    dest.write(opts, target.clone(), target, c.date, c.len, |w| copy_range(disk, c.offset, c.len, w))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{Extent, FileData};
    use crate::source::{MemSource, Source};

    fn file(offset: u64, size: u64) -> DeletedFile {
        DeletedFile {
            id: 0,
            path: "f".into(),
            size,
            created: None,
            modified: None,
            condition: Condition::Recoverable,
            note: None,
            data: FileData::Extents(vec![Extent { offset: Some(offset), len: size }]),
        }
    }

    #[test]
    fn a_fragmented_video_is_rebuilt_by_a_deep_search() {
        use crate::carve::bmff::testing::mp4;
        const CLUSTER: usize = 4096;
        // A video in four pieces and a photo-less video in one piece, on a
        // disk without a file system, between data that is no video.
        let sizes: Vec<usize> = (0..48).map(|i| 4000 + (i * 3571) % 9000).collect();
        let video = mp4(&sizes);
        let whole = mp4(&[3000; 8]);
        let mut disk: Vec<u8> = (0..400 * CLUSTER).map(|i| (i as u8).wrapping_mul(29) | 0x80).collect();
        let clusters: Vec<&[u8]> = video.chunks(CLUSTER).collect();
        let n = clusters.len();
        let starts = [20, 90, 160, 230];
        for (k, c) in clusters.iter().enumerate() {
            let piece = k * 4 / n;
            let at = (starts[piece] + k - piece * n / 4) * CLUSTER;
            disk[at..at + c.len()].copy_from_slice(c);
        }
        disk[320 * CLUSTER..320 * CLUSTER + whole.len()].copy_from_slice(&whole);
        let disk: Source = Arc::new(MemSource(disk));
        let session = Session::new(disk.clone(), "test".into(), partition::discover(&disk));
        let opts = ScanOptions {
            method: Method::Carve,
            partition: None,
            filter: Filter::new(&[], &[], None, 0, None).unwrap(),
            carve_all_space: false,
            step: 512,
            max_carve_size: None,
        };
        let found = scan(&session, &opts, &crate::progress::Silent, &AtomicBool::new(false)).unwrap();
        // The fragmented one, rebuilt.
        assert_eq!(found.fs.len(), 1, "{:?}", found.fs.iter().map(|f| &f.file.path).collect::<Vec<_>>());
        let f = &found.fs[0];
        assert_eq!(f.file.path, format!("f{:012x}.mp4", 20 * CLUSTER));
        assert_eq!(f.file.condition, Condition::Recoverable);
        assert_eq!(session.partition(f.partition).unwrap().label(), "found1_rebuilt_videos");
        assert_eq!(read_item(&session, Item::Fs(f), 1 << 30).unwrap().unwrap(), video);
        assert_eq!(crate::verify::check(&session, Item::Fs(f)), crate::verify::Verdict::Verified);
        // The whole one, found by the deep search as before.
        assert_eq!(found.carved.len(), 1);
        assert_eq!((found.carved[0].offset, found.carved[0].len), ((320 * CLUSTER) as u64, whole.len() as u64));
    }

    #[test]
    fn files_erased_by_the_drive_are_detected() {
        // 0..64K: data; 64K..: zeros (as TRIM leaves freed space).
        let mut disk = vec![0u8; 1 << 20];
        disk[..65536].iter_mut().enumerate().for_each(|(i, b)| *b = (i % 251) as u8 + 1);
        let vol = MemSource(disk);
        // Mostly erased files: the drive erases, so every file is checked.
        let mut files: Vec<DeletedFile> = (0..10).map(|i| file(65536 + i * 16384, 16384)).collect();
        files.push(file(0, 16384));
        assert!(mark_erased(&vol, &mut files));
        assert!(files[..10].iter().all(|f| f.condition == Condition::Erased));
        assert_eq!(files[10].condition, Condition::Recoverable);
    }

    #[test]
    fn a_few_empty_files_do_not_mean_the_drive_erases() {
        let mut disk = vec![0u8; 1 << 20];
        disk[..900_000].iter_mut().enumerate().for_each(|(i, b)| *b = (i % 251) as u8 + 1);
        let vol = MemSource(disk);
        let mut files: Vec<DeletedFile> = (0..10).map(|i| file(i * 16384, 16384)).collect();
        files.push(file(950_000, 16384));
        assert!(!mark_erased(&vol, &mut files));
        assert!(files.iter().all(|f| f.condition == Condition::Recoverable));
    }

    #[test]
    fn data_is_checked_at_start_middle_and_end() {
        let mut disk = vec![0u8; 1 << 20];
        // Zero at the start and end, data in the middle: not erased.
        disk[30_000] = 7;
        let vol = MemSource(disk);
        assert!(!reads_as_zeros(&vol, &file(0, 60_000)));
        assert!(reads_as_zeros(&vol, &file(100_000, 60_000)));
        // Nothing mapped: no verdict.
        let mut lost = file(0, 60_000);
        lost.data = FileData::Extents(Vec::new());
        assert!(!reads_as_zeros(&vol, &lost));
    }
}
