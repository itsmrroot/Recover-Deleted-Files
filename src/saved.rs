//! Saving the results of a scan to a file and opening them later, so that
//! a long scan does not have to be repeated (`.wdfrscan`, JSON).
//!
//! The file records where every file's data lies, plus the size and
//! partition layout of the source, which are checked when it is opened:
//! results only make sense on the very same drive or disk image.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::carve::{self, Carved, Category};
use crate::fs::{Condition, DeletedFile, Extent, FileData, FsKind};
use crate::partition::{Partition, Scheme};
use crate::recover::{Found, FsFound, ItemRef, Session};

/// File extension of saved scans.
pub const EXTENSION: &str = "wdfrscan";
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct SavedScan {
    version: u32,
    /// The drive or disk image the scan was made on.
    pub source: String,
    source_size: u64,
    /// (start, length) of every partition, to recognise the same source.
    partitions: Vec<(u64, u64)>,
    fs: Vec<SavedFs>,
    carved: Vec<SavedCarved>,
    duplicates: Vec<(ItemRef, ItemRef)>,
    erased_by_drive: bool,
    cancelled: bool,
    /// Partitions the deep search found (numbered after the table's).
    #[serde(default)]
    found_partitions: Vec<SavedPartition>,
}

#[derive(Serialize, Deserialize)]
struct SavedPartition {
    index: usize,
    start: u64,
    len: u64,
    kind: String,
    fs: Option<FsKind>,
    /// A Windows shadow copy's device.
    #[serde(default)]
    device: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SavedFs {
    partition: usize,
    id: u64,
    path: String,
    size: u64,
    created: Option<NaiveDateTime>,
    modified: Option<NaiveDateTime>,
    condition: SavedCondition,
    note: Option<String>,
    data: SavedData,
}

#[derive(Serialize, Deserialize)]
enum SavedCondition {
    Recoverable,
    Partial(u8),
    Overwritten,
    Erased,
}

#[derive(Serialize, Deserialize)]
enum SavedData {
    Resident(Vec<u8>),
    Extents(Vec<(Option<u64>, u64)>),
    Compressed { extents: Vec<(Option<u64>, u64)>, unit: u64 },
    Lost,
}

#[derive(Serialize, Deserialize)]
struct SavedCarved {
    offset: u64,
    len: u64,
    ext: String,
    category: Category,
    format: String,
    title: Option<String>,
    date: Option<NaiveDateTime>,
}

fn extents_out(e: &[Extent]) -> Vec<(Option<u64>, u64)> {
    e.iter().map(|e| (e.offset, e.len)).collect()
}

fn extents_in(e: Vec<(Option<u64>, u64)>) -> Vec<Extent> {
    e.into_iter().map(|(offset, len)| Extent { offset, len }).collect()
}

fn layout(session: &Session) -> Vec<(u64, u64)> {
    session.partitions.iter().map(|p| (p.start, p.len)).collect()
}

/// Writes the results of a scan of `session` to `path`.
pub fn save(path: &Path, session: &Session, found: &Found) -> Result<()> {
    let scan = SavedScan {
        version: VERSION,
        source: session.path.clone(),
        source_size: session.disk.size(),
        partitions: layout(session),
        fs: found
            .fs
            .iter()
            .map(|f| SavedFs {
                partition: f.partition,
                id: f.file.id,
                path: f.file.path.clone(),
                size: f.file.size,
                created: f.file.created,
                modified: f.file.modified,
                condition: match f.file.condition {
                    Condition::Recoverable => SavedCondition::Recoverable,
                    Condition::Partial(p) => SavedCondition::Partial(p),
                    Condition::Overwritten => SavedCondition::Overwritten,
                    Condition::Erased => SavedCondition::Erased,
                },
                note: f.file.note.clone(),
                data: match &f.file.data {
                    FileData::Resident(v) => SavedData::Resident(v.clone()),
                    FileData::Extents(e) => SavedData::Extents(extents_out(e)),
                    FileData::Compressed { extents, unit } => {
                        SavedData::Compressed { extents: extents_out(extents), unit: *unit }
                    }
                    FileData::Lost => SavedData::Lost,
                },
            })
            .collect(),
        carved: found
            .carved
            .iter()
            .map(|c| SavedCarved {
                offset: c.offset,
                len: c.len,
                ext: c.ext.to_string(),
                category: c.category,
                format: c.format.to_string(),
                title: c.title.clone(),
                date: c.date,
            })
            .collect(),
        duplicates: found.duplicates.iter().map(|(a, b)| (*a, *b)).collect(),
        erased_by_drive: found.erased_by_drive,
        cancelled: found.cancelled,
        found_partitions: session
            .found_partitions()
            .into_iter()
            .map(|p| SavedPartition {
                index: p.index,
                start: p.start,
                len: p.len,
                kind: p.kind,
                fs: p.fs,
                device: p.device,
            })
            .collect(),
    };
    // Written next to the target and renamed, so a failure never leaves a
    // half-written file under the chosen name.
    let tmp = path.with_extension(format!("{EXTENSION}.tmp"));
    let file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut w = BufWriter::new(file);
    serde_json::to_writer(&mut w, &scan).context("writing the saved scan")?;
    w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

/// Reads a saved scan. Open its [`SavedScan::source`] with
/// [`Session::open`], then call [`SavedScan::into_found`].
pub fn load(path: &Path) -> Result<SavedScan> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let scan: SavedScan =
        serde_json::from_reader(BufReader::new(file)).context("this is not a saved scan, or it is damaged")?;
    ensure!(scan.version == VERSION, "this scan was saved by a newer version of the app");
    Ok(scan)
}

impl SavedScan {
    /// The results, after checking that `session` is the source they were
    /// made on.
    pub fn into_found(self, session: &Session) -> Result<Found> {
        ensure!(
            session.disk.size() == self.source_size && layout(session) == self.partitions,
            "this scan was saved for another drive or disk image ({}), or the drive has changed since",
            self.source
        );
        let formats = carve::all_formats();
        let mut carved = Vec::with_capacity(self.carved.len());
        for c in self.carved {
            // Extension and format names are static in the program.
            let Some(f) = formats.iter().find(|f| f.name() == c.format) else {
                bail!("unknown file format {:?} in the saved scan", c.format);
            };
            let Some((ext, _)) = f.kinds().iter().find(|(e, _)| *e == c.ext) else {
                bail!("unknown file type {:?} in the saved scan", c.ext);
            };
            carved.push(Carved {
                offset: c.offset,
                len: c.len,
                ext,
                category: c.category,
                format: f.name(),
                title: c.title,
                date: c.date,
            });
        }
        for p in self.found_partitions {
            ensure!(
                p.device.is_some() || p.start < session.disk.size(),
                "the saved scan refers to a partition that does not exist"
            );
            session.add_found(Partition {
                index: p.index,
                start: p.start,
                len: p.len,
                scheme: Scheme::Found,
                kind: p.kind,
                name: String::new(),
                fs: p.fs,
                device: p.device,
            });
        }
        let parts = session.partitions.len() + session.found_partitions().len();
        let mut fs = Vec::with_capacity(self.fs.len());
        for f in self.fs {
            ensure!(f.partition < parts, "the saved scan refers to a partition that does not exist");
            fs.push(FsFound {
                partition: f.partition,
                file: DeletedFile {
                    id: f.id,
                    path: f.path,
                    size: f.size,
                    created: f.created,
                    modified: f.modified,
                    condition: match f.condition {
                        SavedCondition::Recoverable => Condition::Recoverable,
                        SavedCondition::Partial(p) => Condition::Partial(p),
                        SavedCondition::Overwritten => Condition::Overwritten,
                        SavedCondition::Erased => Condition::Erased,
                    },
                    note: f.note,
                    data: match f.data {
                        SavedData::Resident(v) => FileData::Resident(v),
                        SavedData::Extents(e) => FileData::Extents(extents_in(e)),
                        SavedData::Compressed { extents, unit } => {
                            FileData::Compressed { extents: extents_in(extents), unit }
                        }
                        SavedData::Lost => FileData::Lost,
                    },
                },
            });
        }
        let valid = |r: &ItemRef| match r {
            ItemRef::Fs(i) => *i < fs.len(),
            ItemRef::Carved(i) => *i < carved.len(),
        };
        ensure!(self.duplicates.iter().all(|(a, b)| valid(a) && valid(b)), "the saved scan is damaged");
        Ok(Found {
            duplicates: self.duplicates.into_iter().collect(),
            fs,
            carved,
            cancelled: self.cancelled,
            erased_by_drive: self.erased_by_drive,
        })
    }
}
