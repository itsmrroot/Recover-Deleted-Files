//! Recovery orchestration: file-system recovery, then carving of the space
//! that is still unaccounted for, with a CSV report of everything written.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};

use crate::carve::{self, CarveOptions, Carved};
use crate::filter::Filter;
use crate::fs::{self, Condition, DeletedFile};
use crate::output::{self, Report, ReportRow};
use crate::partition::{self, Partition};
use crate::ranges::{self, ByteRange};
use crate::source::{DiskSource, ReadAt, Source, read_tolerant};
use crate::units::format_size;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Method {
    /// File-system metadata first, then carve the remaining free space.
    All,
    /// Only file-system metadata (keeps original names and folders).
    Fs,
    /// Only signature carving (works on formatted or unknown file systems).
    Carve,
}

pub struct Options {
    pub out: PathBuf,
    pub method: Method,
    pub partition: Option<usize>,
    pub filter: Filter,
    pub include_overwritten: bool,
    /// Carve the whole disk instead of only unallocated space.
    pub carve_all_space: bool,
    pub step: u64,
    pub max_carve_size: Option<u64>,
    pub allow_same_volume: bool,
    pub quiet: bool,
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
}

/// An opened file system and the deleted files found on it.
pub type ScanResult = (Box<dyn fs::Volume>, Vec<DeletedFile>);

/// Deleted files on one partition, filtered. `None` if the partition has
/// no supported file system.
pub fn scan_partition(session: &Session, p: &Partition, filter: &Filter, quiet: bool) -> Result<Option<ScanResult>> {
    if p.fs.is_none() {
        return Ok(None);
    }
    let vol = fs::open(p.source(&session.disk)).with_context(|| format!("opening {}", p.label()))?;
    let pb = bar(quiet, 0, &format!("{} metadata", p.label()), "{msg:30} [{bar:40}] {pos}/{len} ({eta})");
    let mut files = vol.scan_deleted(&mut |done, total| {
        pb.set_length(total);
        pb.set_position(done);
    })?;
    pb.finish_and_clear();
    files.retain(|f| filter.matches_file(f));
    Ok(Some((vol, files)))
}

pub fn run(session: &Session, opts: &Options, cancel: &AtomicBool) -> Result<Summary> {
    std::fs::create_dir_all(&opts.out).with_context(|| format!("creating {}", opts.out.display()))?;
    if !opts.allow_same_volume {
        output::ensure_not_on_source(&session.path, &opts.out)?;
    }
    let mut report = Report::create(&opts.out)?;
    let mut sum = Summary::default();
    let parts = session.selected(opts.partition)?;
    let mut claimed: Vec<ByteRange> = Vec::new();
    let mut free: Vec<ByteRange> = Vec::new();

    for p in &parts {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let vol = if opts.method == Method::Carve {
            p.fs.and_then(|_| fs::open(p.source(&session.disk)).ok())
        } else {
            match scan_partition(session, p, &opts.filter, opts.quiet) {
                Ok(Some((vol, files))) => {
                    recover_files(p, vol.as_ref(), &files, opts, cancel, &mut report, &mut sum, &mut claimed)?;
                    Some(vol)
                }
                Ok(None) => None,
                Err(e) => {
                    log::warn!("{}: file-system scan failed: {e:#}", p.label());
                    None
                }
            }
        };
        // Work out which part of this partition is worth carving.
        if opts.method != Method::Fs {
            let unalloc = match (&vol, opts.carve_all_space) {
                (Some(v), false) => v.free_ranges().map_err(|e| {
                    log::warn!("{}: no allocation map ({e:#}); carving the whole partition", p.label());
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
        carve_ranges(session, &todo, opts, cancel, &mut report, &mut sum)?;
    }

    sum.cancelled = cancel.load(Ordering::Relaxed);
    sum.report = Some(report.finish()?);
    Ok(sum)
}

#[allow(clippy::too_many_arguments)]
fn recover_files(
    p: &Partition,
    vol: &dyn fs::Volume,
    files: &[DeletedFile],
    opts: &Options,
    cancel: &AtomicBool,
    report: &mut Report,
    sum: &mut Summary,
    claimed: &mut Vec<ByteRange>,
) -> Result<()> {
    let wanted: Vec<&DeletedFile> = files
        .iter()
        .filter(|f| {
            let keep = opts.include_overwritten || f.condition != Condition::Overwritten;
            if !keep {
                sum.skipped_overwritten += 1;
            }
            keep
        })
        .collect();
    let total: u64 = wanted.iter().map(|f| f.size).sum();
    let pb = bar(opts.quiet, total, &format!("{} files", p.label()), BYTES_STYLE);
    let base = opts.out.join(p.label());
    for f in wanted {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        pb.set_message(f.name().to_string());
        match write_file(vol.source().as_ref(), f, &base) {
            Ok((path, st)) => {
                sum.fs_files += 1;
                sum.fs_bytes += st.written;
                sum.unreadable_bytes += st.unreadable;
                if f.condition == Condition::Recoverable {
                    claimed.extend(f.data_ranges().into_iter().map(|r| p.start + r.start..p.start + r.end));
                }
                report.add(&ReportRow {
                    method: "filesystem",
                    partition: p.label(),
                    original_path: f.path.clone(),
                    recovered_path: rel(&opts.out, &path),
                    size: f.size,
                    disk_offset: f
                        .data_ranges()
                        .first()
                        .map(|r| format!("{:#x}", p.start + r.start))
                        .unwrap_or_default(),
                    condition: f.condition.to_string(),
                    modified: f.modified.map(|t| t.to_string()).unwrap_or_default(),
                    note: f.note.clone().unwrap_or_default(),
                    unreadable_bytes: st.unreadable,
                })?;
            }
            Err(e) => {
                sum.failures += 1;
                pb.suspend(|| log::error!("{}: {e:#}", f.path));
            }
        }
        pb.inc(f.size);
    }
    pb.finish_and_clear();
    Ok(())
}

fn write_file(vol: &dyn ReadAt, f: &DeletedFile, base: &Path) -> Result<(PathBuf, fs::ExtractStats)> {
    let target = base.join(output::safe_relative_path(&f.path));
    // Deep or exotic paths can still fail on some systems; fall back to a
    // flat name rather than losing the file.
    let (path, file) = output::create_unique(target).or_else(|_| {
        output::create_unique(base.join("_flat").join(format!("{}_{}", f.id, output::sanitize_component(f.name()))))
    })?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    let st = fs::extract(vol, f, &mut w).with_context(|| format!("writing {}", path.display()))?;
    let file = w.into_inner().map_err(|e| e.into_error())?;
    if let Some(m) = f.modified {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(m.and_utc().timestamp().max(0) as u64);
        let _ = file.set_modified(t);
    }
    Ok((path, st))
}

fn carve_ranges(
    session: &Session,
    todo: &[ByteRange],
    opts: &Options,
    cancel: &AtomicBool,
    report: &mut Report,
    sum: &mut Summary,
) -> Result<()> {
    let formats: Vec<_> = carve::all_formats().into_iter().filter(|f| opts.filter.wants_format(*f)).collect();
    if formats.is_empty() || todo.is_empty() {
        return Ok(());
    }
    let total = ranges::total(todo);
    if !opts.quiet {
        eprintln!("Carving {} in {} region(s) for {} format(s)...", format_size(total), todo.len(), formats.len());
    }
    let pb = bar(opts.quiet, total, "carving", BYTES_STYLE);
    let filter = &opts.filter;
    let carve_opts = CarveOptions { formats, step: opts.step, max_size: opts.max_carve_size };
    let disk = session.disk.as_ref();
    let dir = opts.out.join("carved");
    let mut found = 0u64;
    let stats = carve::carve(
        disk,
        todo,
        &carve_opts,
        cancel,
        |c: &Carved| {
            // Files rejected by size are still skipped over by the scanner,
            // so nothing inside them is mistaken for another file.
            if !filter.matches_carved(c) {
                return Ok(());
            }
            let target = dir.join(c.category.dir_name()).join(format!("f{:012x}.{}", c.offset, c.ext));
            let (path, file) = output::create_unique(target)?;
            let mut w = BufWriter::with_capacity(1 << 20, file);
            let unreadable = copy_range(disk, c.offset, c.len, &mut w)?;
            w.flush()?;
            found += 1;
            sum.carved_files += 1;
            sum.carved_bytes += c.len;
            sum.unreadable_bytes += unreadable;
            pb.set_message(format!("{found} files found"));
            report.add(&ReportRow {
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
            })
        },
        |n| pb.inc(n),
    )?;
    log::info!("carving scanned {} bytes, {} candidates accepted", stats.scanned, stats.found);
    pb.finish_and_clear();
    Ok(())
}

fn copy_range(src: &dyn ReadAt, offset: u64, len: u64, out: &mut dyn Write) -> Result<u64> {
    let mut buf = vec![0u8; 1 << 20];
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

const BYTES_STYLE: &str = "{msg:30!} [{bar:40}] {bytes}/{total_bytes} {bytes_per_sec} ({eta})";

fn bar(quiet: bool, len: u64, msg: &str, template: &str) -> ProgressBar {
    if quiet {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_bar()).progress_chars("=> "),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(250));
    pb
}
