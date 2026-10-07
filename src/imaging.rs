//! Copying a whole drive into an image file, for failing drives.
//!
//! A dying drive gets worse with every read, so it is read once, as gently
//! as possible, and everything else (scanning, previews, recovery) uses the
//! copy. Like GNU ddrescue:
//!
//! 1. The first pass copies everything that reads easily. A read error
//!    marks the area for later and jumps ahead, further after every error
//!    in a row, so a damaged zone does not stall the copy.
//! 2. The second pass retries what was skipped, sector by sector, and
//!    marks what still fails as unreadable (zeros in the image).
//!
//! Progress is kept in a map file next to the image (`<image>.map`), so an
//! interrupted copy continues where it stopped.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::progress::{Progress, Unit};
use crate::source::ReadAt;

/// Read size of the first pass.
const CHUNK: u64 = 1 << 20;
/// Furthest jump after a run of read errors.
const MAX_SKIP: u64 = 64 << 20;
/// Read size of the second pass.
const SECTOR: u64 = 512;
const SAVE_EVERY: Duration = Duration::from_secs(2);

/// The state of an area of the drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Not read yet.
    Untried,
    /// Failed in the first pass: retried in the second.
    Skipped,
    Copied,
    /// Could not be read at all: zeros in the image.
    Unreadable,
}

impl State {
    /// The ddrescue map file characters.
    fn code(self) -> char {
        match self {
            State::Untried => '?',
            State::Skipped => '*',
            State::Copied => '+',
            State::Unreadable => '-',
        }
    }

    fn from_code(c: &str) -> Option<Self> {
        Some(match c {
            "?" => State::Untried,
            "*" | "/" => State::Skipped,
            "+" => State::Copied,
            "-" => State::Unreadable,
            _ => return None,
        })
    }
}

/// What is known about every byte of the drive: sorted, adjacent areas
/// covering `0..size` exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct Map {
    pub size: u64,
    areas: Vec<(u64, u64, State)>,
}

impl Map {
    pub fn new(size: u64) -> Self {
        Self { size, areas: if size == 0 { Vec::new() } else { vec![(0, size, State::Untried)] } }
    }

    /// Marks `start..start+len` (clipped to the drive) as `state`.
    pub fn set(&mut self, start: u64, len: u64, state: State) {
        let end = (start + len).min(self.size);
        if start >= end {
            return;
        }
        let mut out = Vec::with_capacity(self.areas.len() + 2);
        for &(s, l, st) in &self.areas {
            let e = s + l;
            if e <= start || s >= end {
                out.push((s, l, st));
                continue;
            }
            if s < start {
                out.push((s, start - s, st));
            }
            if e > end {
                out.push((end, e - end, st));
            }
        }
        out.push((start, end - start, state));
        out.sort_by_key(|a| a.0);
        // Merge neighbours in the same state.
        let mut merged: Vec<(u64, u64, State)> = Vec::with_capacity(out.len());
        for a in out {
            match merged.last_mut() {
                Some(last) if last.2 == a.2 && last.0 + last.1 == a.0 => last.1 += a.1,
                _ => merged.push(a),
            }
        }
        self.areas = merged;
    }

    /// The first area in `state`, if any.
    fn first(&self, state: State) -> Option<(u64, u64)> {
        self.areas.iter().find(|a| a.2 == state).map(|a| (a.0, a.1))
    }

    /// Bytes in `state`.
    pub fn total(&self, state: State) -> u64 {
        self.areas.iter().filter(|a| a.2 == state).map(|a| a.1).sum()
    }

    pub fn areas(&self) -> &[(u64, u64, State)] {
        &self.areas
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        // Written aside and renamed, so a crash never leaves half a map.
        let tmp = path.with_extension("map.tmp");
        let mut f = io::BufWriter::new(File::create(&tmp)?);
        writeln!(f, "# Deleted Files Recovery: drive copy map (ddrescue format)")?;
        writeln!(f, "# size {}", self.size)?;
        // ddrescue's status line: where to continue, the phase, the pass.
        let next = self.areas.iter().find(|a| a.2 != State::Copied).map_or(self.size, |a| a.0);
        writeln!(f, "# current_pos  current_status  current_pass")?;
        writeln!(f, "0x{next:08X}     ?               1")?;
        writeln!(f, "#      pos        size  status")?;
        for &(s, l, st) in &self.areas {
            writeln!(f, "0x{s:08X}  0x{l:08X}  {}", st.code())?;
        }
        f.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(tmp, path)
    }

    fn load(path: &Path, size: u64) -> Option<Self> {
        let file = File::open(path).ok()?;
        let mut map = Map::new(size);
        let mut covered = 0u64;
        for line in BufReader::new(file).lines() {
            let line = line.ok()?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            let num = |s: &str| u64::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16).ok();
            // Skips the status line ("pos status pass"): its second field
            // is not a size.
            let (Some(start), Some(len), Some(state)) = (
                parts.first().and_then(|p| num(p)),
                parts.get(1).and_then(|p| num(p)),
                parts.get(2).and_then(|p| State::from_code(p)),
            ) else {
                continue;
            };
            map.set(start, len, state);
            covered += len;
        }
        (covered == size).then_some(map)
    }
}

/// The map file that belongs to an image.
pub fn map_path(image: &Path) -> PathBuf {
    let mut name = image.file_name().unwrap_or_default().to_os_string();
    name.push(".map");
    image.with_file_name(name)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub size: u64,
    pub copied: u64,
    pub unreadable: u64,
    /// Continued an earlier, interrupted copy.
    pub resumed: bool,
    pub cancelled: bool,
}

/// Copies `src` into the image file `out`, or continues an earlier copy
/// recorded in its map. Unreadable areas are left as zeros.
pub fn copy(src: &dyn ReadAt, out: &Path, progress: &dyn Progress, cancel: &AtomicBool) -> Result<Stats> {
    let size = src.size();
    if size == 0 {
        bail!("the source is empty");
    }
    let map_file = map_path(out);
    let existing = std::fs::metadata(out).ok().map(|m| m.len());
    let (mut map, resumed) = match (existing, Map::load(&map_file, size)) {
        (Some(len), Some(map)) if len == size => (map, true),
        _ => (Map::new(size), false),
    };
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(out)
        .with_context(|| format!("creating {}", out.display()))?;
    file.set_len(size)
        .with_context(|| format!("making room for {} in {}", crate::units::format_size(size), out.display()))?;

    progress.begin("Copying the drive", size, Unit::Bytes);
    let mut saved = Instant::now();
    let mut buf = vec![0u8; CHUNK as usize];
    let report = |map: &Map| {
        progress.set(map.total(State::Copied) + map.total(State::Unreadable), size);
        let bad = map.total(State::Skipped) + map.total(State::Unreadable);
        if bad > 0 {
            progress.item(&format!("{} not readable yet", crate::units::format_size(bad)));
        }
    };
    report(&map);

    // Pass 1: everything that reads easily.
    let mut skip = CHUNK;
    while let Some((start, len)) = map.first(State::Untried) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // Whole chunks on chunk boundaries keep device reads aligned.
        let n = len.min(CHUNK - start % CHUNK);
        match src.read_exact_at(start, &mut buf[..n as usize]) {
            Ok(()) => {
                write_at(&file, start, &buf[..n as usize])?;
                map.set(start, n, State::Copied);
                skip = CHUNK;
            }
            Err(_) => {
                // Leave the bad area for later and jump past it, further
                // after every error in a row.
                map.set(start, n.max(skip).min(len), State::Skipped);
                skip = (skip * 2).min(MAX_SKIP);
            }
        }
        if saved.elapsed() > SAVE_EVERY {
            map.save(&map_file)?;
            saved = Instant::now();
        }
        report(&map);
    }

    // Pass 2: retry the skipped areas sector by sector.
    progress.begin("Retrying damaged areas", size, Unit::Bytes);
    report(&map);
    while let Some((start, _)) = map.first(State::Skipped) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let n = SECTOR.min(size - start).min(SECTOR - start % SECTOR);
        let state = match src.read_exact_at(start, &mut buf[..n as usize]) {
            Ok(()) => {
                write_at(&file, start, &buf[..n as usize])?;
                State::Copied
            }
            Err(_) => State::Unreadable,
        };
        map.set(start, n, state);
        if saved.elapsed() > SAVE_EVERY {
            map.save(&map_file)?;
            saved = Instant::now();
        }
        report(&map);
    }
    progress.end();
    file.sync_all()?;
    map.save(&map_file)?;
    Ok(Stats {
        size,
        copied: map.total(State::Copied),
        unreadable: map.total(State::Unreadable),
        resumed,
        cancelled: cancel.load(Ordering::Relaxed),
    })
}

fn write_at(file: &File, offset: u64, data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(data, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < data.len() {
            done += file.seek_write(&data[done..], offset + done as u64)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Silent;

    /// A drive with unreadable byte ranges.
    struct Damaged {
        data: Vec<u8>,
        bad: Vec<std::ops::Range<u64>>,
    }

    impl ReadAt for Damaged {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let end = offset + buf.len() as u64;
            if self.bad.iter().any(|b| b.start < end && offset < b.end) {
                return Err(io::Error::other("bad sector"));
            }
            let Some(rest) = self.data.get(offset as usize..) else { return Ok(0) };
            let n = buf.len().min(rest.len());
            buf[..n].copy_from_slice(&rest[..n]);
            Ok(n)
        }
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
    }

    fn drive() -> Damaged {
        let data: Vec<u8> = (0..5 * CHUNK as usize + 3000).map(|i| (i % 251) as u8 + 1).collect();
        // Two bad sectors in the second megabyte, one near the end.
        let bad = vec![CHUNK + 4096..CHUNK + 5120, 5 * CHUNK + 512..5 * CHUNK + 1024];
        Damaged { data, bad }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wdfr-imaging-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("drive.img")
    }

    #[test]
    fn copies_around_bad_sectors() {
        let src = drive();
        let out = temp("copy");
        let st = copy(&src, &out, &Silent, &AtomicBool::new(false)).unwrap();
        assert_eq!(st.unreadable, 1024 + 512);
        assert_eq!(st.copied, src.size() - st.unreadable);
        let img = std::fs::read(&out).unwrap();
        assert_eq!(img.len(), src.data.len());
        for (i, (a, b)) in img.iter().zip(&src.data).enumerate() {
            let bad = src.bad.iter().any(|r| r.contains(&(i as u64)));
            assert_eq!(*a, if bad { 0 } else { *b }, "byte {i}");
        }
        let map = Map::load(&map_path(&out), src.size()).unwrap();
        assert_eq!(map.total(State::Unreadable), 1536);
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }

    #[test]
    fn an_interrupted_copy_continues() {
        let src = drive();
        let out = temp("resume");
        // A copy stopped after the first megabyte.
        let mut map = Map::new(src.size());
        map.set(0, CHUNK, State::Copied);
        std::fs::write(&out, vec![0u8; src.data.len()]).unwrap();
        map.save(&map_path(&out)).unwrap();
        // The first megabyte is not read again: it is wrong in the image on
        // purpose (zeros), and stays so.
        let st = copy(&src, &out, &Silent, &AtomicBool::new(false)).unwrap();
        assert!(st.resumed);
        let img = std::fs::read(&out).unwrap();
        assert!(img[..CHUNK as usize].iter().all(|&b| b == 0));
        assert_eq!(img[CHUNK as usize..CHUNK as usize + 4096], src.data[CHUNK as usize..CHUNK as usize + 4096]);
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }

    #[test]
    fn map_areas_split_and_merge() {
        let mut m = Map::new(100);
        m.set(10, 20, State::Copied);
        m.set(30, 10, State::Copied);
        m.set(15, 5, State::Unreadable);
        assert_eq!(
            m.areas(),
            &[
                (0, 10, State::Untried),
                (10, 5, State::Copied),
                (15, 5, State::Unreadable),
                (20, 20, State::Copied),
                (40, 60, State::Untried)
            ]
        );
        assert_eq!(m.total(State::Copied), 25);
    }
}
