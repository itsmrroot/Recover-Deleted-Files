//! NTFS deleted-file recovery.
//!
//! When NTFS deletes a file it clears the "in use" flag of its MFT record and
//! the clusters' bits in `$Bitmap`, but leaves the record content — name,
//! parent directory, timestamps and the data run list — intact until the
//! record is reused. We walk every MFT record, pick the freed ones and
//! rebuild their paths from the parent references.

pub mod lznt1;
pub mod record;

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};

use self::record::{ATTR_DATA, Data, FileRecord, NonResident, Run, apply_fixups, ref_record, ref_seq};
use super::{Condition, DeletedFile, Extent, FileData, FsKind, Volume, filetime};
use crate::bytes::{le16, le64, u8_at};
use crate::ranges::{ByteRange, bit_is_set, zero_bit_runs};
use crate::source::{Source, read_tolerant};

const ROOT_RECORD: u64 = 5;
const BITMAP_RECORD: u64 = 6;
/// Records below this are reserved for metadata files.
const FIRST_USER_RECORD: u64 = 24;

pub struct Ntfs {
    src: Source,
    cluster: u64,
    record_size: u64,
    total_clusters: u64,
    /// Where the `$MFT` stream lives on the volume.
    mft: Vec<Extent>,
    mft_bytes: u64,
}

/// What we remember about every record, to rebuild paths.
struct Node {
    seq: u16,
    in_use: bool,
    parent: u64,
    name: Box<str>,
}

impl Ntfs {
    pub fn open(src: Source) -> Result<Self> {
        let bs = src.read_vec(0, 512).context("reading NTFS boot sector")?;
        ensure!(&bs[3..11] == b"NTFS    ", "not an NTFS boot sector");
        let bps = u64::from(le16(&bs, 0x0B).unwrap_or(0));
        let spc_raw = u8_at(&bs, 0x0D).unwrap_or(0);
        let spc = if spc_raw > 0x80 { 1u64 << (256 - u32::from(spc_raw)) } else { u64::from(spc_raw) };
        let cluster = bps * spc;
        ensure!(
            bps.is_power_of_two() && (256..=4096).contains(&bps) && cluster.is_power_of_two() && cluster <= 2 << 20,
            "invalid NTFS geometry (sector {bps}, cluster {cluster})"
        );
        let total_sectors = le64(&bs, 0x28).unwrap_or(0);
        let total_clusters = total_sectors * bps / cluster;
        let cpr = bs[0x40] as i8;
        let record_size = if cpr > 0 { cpr as u64 * cluster } else { 1u64 << u32::from(cpr.unsigned_abs()) };
        ensure!((256..=65536).contains(&record_size), "invalid MFT record size {record_size}");

        let mut ntfs = Ntfs { src, cluster, record_size, total_clusters, mft: Vec::new(), mft_bytes: 0 };
        let mft_lcn = le64(&bs, 0x30).unwrap_or(0);
        let mirr_lcn = le64(&bs, 0x38).unwrap_or(0);
        let rec0 = ntfs
            .read_record_at(mft_lcn * cluster)
            .or_else(|| {
                log::warn!("$MFT record 0 unreadable, trying $MFTMirr");
                ntfs.read_record_at(mirr_lcn * cluster)
            })
            .context("cannot read $MFT record 0 (or its mirror)")?;
        let Some(Data::NonResident(nr)) = rec0.data.clone() else {
            bail!("$MFT has no non-resident $DATA attribute");
        };
        ntfs.mft_bytes = nr.real_size;
        ntfs.mft = runs_to_extents(&nr.runs, cluster);

        // A heavily fragmented $MFT keeps the rest of its run list in
        // extension records referenced from an $ATTRIBUTE_LIST.
        if let Some(list) = &rec0.attr_list {
            let mut frags = vec![nr];
            for e in list.iter().filter(|e| e.attr_type == ATTR_DATA && ref_record(e.record) != 0) {
                if let Some(r) = ntfs.read_record(ref_record(e.record))
                    && let Some(Data::NonResident(f)) = r.data
                {
                    frags.push(f);
                }
            }
            let (runs, _) = merge_fragments(frags);
            ntfs.mft = runs_to_extents(&runs, cluster);
        }
        Ok(ntfs)
    }

    fn record_count(&self) -> u64 {
        self.mft_bytes / self.record_size
    }

    fn read_record_at(&self, offset: u64) -> Option<FileRecord> {
        let mut buf = self.src.read_vec(offset, self.record_size as usize).ok()?;
        apply_fixups(&mut buf);
        FileRecord::parse(&buf)
    }

    fn read_record(&self, no: u64) -> Option<FileRecord> {
        let mut buf = vec![0u8; self.record_size as usize];
        self.read_mft(no * self.record_size, &mut buf);
        let torn = !apply_fixups(&mut buf);
        let mut r = FileRecord::parse(&buf)?;
        r.torn = torn;
        Some(r)
    }

    /// Reads from the logical `$MFT` stream, zero-filling anything unmapped
    /// or unreadable.
    fn read_mft(&self, mut logical: u64, buf: &mut [u8]) {
        let mut done = 0usize;
        let mut ext_start = 0u64;
        for e in &self.mft {
            if done == buf.len() {
                break;
            }
            let ext_end = ext_start + e.len;
            if logical < ext_end {
                let within = logical - ext_start;
                let n = ((e.len - within) as usize).min(buf.len() - done);
                let dst = &mut buf[done..done + n];
                match e.offset {
                    Some(o) => {
                        read_tolerant(self.src.as_ref(), o + within, dst);
                    }
                    None => dst.fill(0),
                }
                done += n;
                logical += n as u64;
            }
            ext_start = ext_end;
        }
        buf[done..].fill(0);
    }

    fn load_bitmap(&self) -> Result<Vec<u8>> {
        let rec = self.read_record(BITMAP_RECORD).context("reading $Bitmap record")?;
        let file = DeletedFile {
            id: BITMAP_RECORD,
            path: "$Bitmap".into(),
            size: 0,
            created: None,
            modified: None,
            condition: Condition::Recoverable,
            note: None,
            data: FileData::Lost,
        };
        let (data, size) = match rec.data {
            Some(Data::NonResident(nr)) => (FileData::Extents(runs_to_extents(&nr.runs, self.cluster)), nr.real_size),
            Some(Data::Resident(v)) => {
                let n = v.len() as u64;
                (FileData::Resident(v), n)
            }
            None => bail!("$Bitmap has no data"),
        };
        let expected = self.total_clusters.div_ceil(8);
        ensure!(size >= expected && size < expected + (64 << 20), "implausible $Bitmap size {size}");
        let file = DeletedFile { size, data, ..file };
        let mut out = Vec::with_capacity(size as usize);
        super::extract(self.src.as_ref(), &file, &mut out)?;
        Ok(out)
    }

    /// Combines a deleted base record with fragments from its extension
    /// records into data + size + note.
    fn assemble(
        &self,
        rec: &FileRecord,
        extensions: Option<&Vec<(u16, NonResident)>>,
    ) -> (FileData, u64, Option<String>) {
        let fname_size = rec.file_name.as_ref().map_or(0, |f| f.real_size);
        let mut frags: Vec<NonResident> = Vec::new();
        match &rec.data {
            Some(Data::Resident(v)) => return (FileData::Resident(v.clone()), v.len() as u64, None),
            Some(Data::NonResident(nr)) => frags.push(nr.clone()),
            None => {}
        }
        if let Some(ext) = extensions {
            // When a record is freed its sequence number is incremented, so
            // extensions point at either the current or the previous value.
            frags.extend(
                ext.iter()
                    .filter(|(seq, _)| *seq == rec.seq || seq.wrapping_add(1) == rec.seq)
                    .map(|(_, nr)| nr.clone()),
            );
        }
        self.data_from_fragments(frags, fname_size)
    }

    /// Turns the run-list fragments of one stream into extraction data.
    fn data_from_fragments(&self, frags: Vec<NonResident>, fallback_size: u64) -> (FileData, u64, Option<String>) {
        let fname_size = fallback_size;
        if frags.is_empty() {
            return if fname_size == 0 {
                (FileData::Resident(Vec::new()), 0, None)
            } else {
                (FileData::Lost, fname_size, Some("data run list lost".into()))
            };
        }
        let first = frags.iter().find(|f| f.start_vcn == 0).cloned();
        let size = first.as_ref().map_or(fname_size, |f| f.real_size);
        let (runs, complete) = merge_fragments(frags);
        let mut notes = Vec::new();
        if !complete {
            notes.push("some data runs missing (zero-filled)");
        }
        let (compressed, encrypted, cu) =
            first.as_ref().map_or((false, false, 4), |f| (f.compressed, f.encrypted, f.compression_unit));
        if encrypted {
            notes.push("EFS-encrypted: recovered content is ciphertext");
        }
        let extents = runs_to_extents(&runs, self.cluster);
        let data = if compressed {
            let cu = if cu == 0 { 4 } else { cu };
            FileData::Compressed { extents, unit: self.cluster << cu }
        } else {
            FileData::Extents(extents)
        };
        let note = (!notes.is_empty()).then(|| notes.join("; "));
        (data, size, note)
    }

    fn condition(&self, data: &FileData, size: u64, bitmap: Option<&[u8]>) -> Condition {
        let extents = match data {
            FileData::Lost => return Condition::Overwritten,
            FileData::Resident(_) => return Condition::Recoverable,
            FileData::Extents(e) | FileData::Compressed { extents: e, .. } => e,
        };
        let Some(bitmap) = bitmap else { return Condition::Recoverable };
        let mut needed = size.div_ceil(self.cluster);
        let (mut free, mut total) = (0u64, 0u64);
        for e in extents {
            if needed == 0 {
                break;
            }
            let clusters = (e.len / self.cluster).min(needed);
            needed -= clusters;
            let Some(off) = e.offset else { continue };
            let lcn = off / self.cluster;
            if lcn + clusters > self.total_clusters {
                return Condition::Overwritten; // run list points outside the volume
            }
            for c in lcn..lcn + clusters {
                total += 1;
                if bit_is_set(bitmap, c) == Some(false) {
                    free += 1;
                }
            }
        }
        Condition::from_counts(free, total)
    }
}

impl Volume for Ntfs {
    fn kind(&self) -> FsKind {
        FsKind::Ntfs
    }

    fn describe(&self) -> String {
        format!("NTFS, {} clusters, {} MFT records", crate::units::format_size(self.cluster), self.record_count())
    }

    fn source(&self) -> &Source {
        &self.src
    }

    fn scan_deleted(&self, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<DeletedFile>> {
        let count = self.record_count();
        let rs = self.record_size as usize;
        let per_batch = ((4 << 20) / rs).max(1) as u64;
        let mut nodes: Vec<Option<Node>> = Vec::with_capacity(count as usize);
        let mut candidates: Vec<(u64, FileRecord)> = Vec::new();
        let mut extensions: HashMap<u64, Vec<(u16, NonResident)>> = HashMap::new();
        let mut buf = vec![0u8; per_batch as usize * rs];

        let mut no = 0u64;
        while no < count {
            let n = per_batch.min(count - no);
            let batch = &mut buf[..n as usize * rs];
            self.read_mft(no * self.record_size, batch);
            for raw in batch.chunks_exact_mut(rs) {
                let torn = !apply_fixups(raw);
                let rec = FileRecord::parse(raw);
                let mut node = None;
                if let Some(mut rec) = rec {
                    rec.torn = torn;
                    if rec.base_ref != 0 {
                        if let Some(Data::NonResident(nr)) = rec.data.take() {
                            extensions.entry(ref_record(rec.base_ref)).or_default().push((ref_seq(rec.base_ref), nr));
                        }
                    } else if let Some(f) = &rec.file_name {
                        node = Some(Node {
                            seq: rec.seq,
                            in_use: rec.in_use(),
                            parent: f.parent,
                            name: f.name.clone().into_boxed_str(),
                        });
                        if !rec.in_use() && !rec.is_dir() && no >= FIRST_USER_RECORD {
                            candidates.push((no, rec));
                        }
                    }
                }
                nodes.push(node);
                no += 1;
            }
            progress(no, count);
        }

        let bitmap = match self.load_bitmap() {
            Ok(b) => Some(b),
            Err(e) => {
                log::warn!("cannot read $Bitmap, overwrite detection disabled: {e:#}");
                None
            }
        };

        let mut cache: HashMap<u64, String> = HashMap::new();
        let mut out = Vec::with_capacity(candidates.len());
        for (no, rec) in candidates {
            let Some(fname) = rec.file_name.as_ref() else { continue };
            let (data, size, mut note) = self.assemble(&rec, extensions.get(&no));
            let condition = self.condition(&data, size, bitmap.as_deref());
            if rec.torn {
                note = Some(match note {
                    Some(n) => format!("{n}; torn MFT record"),
                    None => "torn MFT record".into(),
                });
            }
            let dir = dir_path(&nodes, fname.parent, &mut cache);
            let path = if dir.is_empty() { fname.name.clone() } else { format!("{dir}/{}", fname.name) };
            let pick = |si: u64, fnt: u64| filetime(if si != 0 { si } else { fnt });
            let created = pick(rec.si_created, fname.created);
            let modified = pick(rec.si_modified, fname.modified);
            // Alternate data streams become separate files: "name:stream".
            for (stream, sdata) in &rec.streams {
                let (data, size, note) = match sdata {
                    Data::Resident(v) => (FileData::Resident(v.clone()), v.len() as u64, None),
                    Data::NonResident(nr) => self.data_from_fragments(vec![nr.clone()], 0),
                };
                out.push(DeletedFile {
                    id: no,
                    path: format!("{path}:{stream}"),
                    size,
                    created,
                    modified,
                    condition: self.condition(&data, size, bitmap.as_deref()),
                    note,
                    data,
                });
            }
            out.push(DeletedFile { id: no, path, size, created, modified, condition, note, data });
        }
        Ok(out)
    }

    fn free_ranges(&self) -> Result<Vec<ByteRange>> {
        let bitmap = self.load_bitmap()?;
        Ok(zero_bit_runs(&bitmap, self.total_clusters)
            .into_iter()
            .map(|r| r.start * self.cluster..r.end * self.cluster)
            .collect())
    }
}

/// Builds the directory path for `parent_ref`, memoised per directory.
fn dir_path(nodes: &[Option<Node>], parent_ref: u64, cache: &mut HashMap<u64, String>) -> String {
    let mut chain: Vec<(u64, &str)> = Vec::new();
    let mut cur = parent_ref;
    let mut prefix: Option<String> = None;
    for _ in 0..1024 {
        if let Some(p) = cache.get(&cur) {
            prefix = Some(p.clone());
            break;
        }
        let no = ref_record(cur);
        if no == ROOT_RECORD {
            prefix = Some(String::new());
            break;
        }
        let seq = ref_seq(cur);
        match nodes.get(no as usize).and_then(Option::as_ref) {
            Some(n) if seq == 0 || n.seq == seq || (!n.in_use && n.seq == seq.wrapping_add(1)) => {
                chain.push((cur, &n.name));
                cur = n.parent;
            }
            // The parent record was reused or never existed: the directory
            // structure is gone, but the file itself may still be fine.
            _ => {
                prefix = Some("$Orphan".into());
                break;
            }
        }
    }
    let mut path = prefix.unwrap_or_else(|| "$Orphan".into());
    for (r, name) in chain.into_iter().rev() {
        path = if path.is_empty() { name.to_string() } else { format!("{path}/{name}") };
        cache.insert(r, path.clone());
    }
    path
}

/// Orders fragments by VCN and concatenates their runs. Gaps are filled
/// with sparse runs; the flag reports whether the list was gap-free.
fn merge_fragments(mut frags: Vec<NonResident>) -> (Vec<Run>, bool) {
    frags.sort_by_key(|f| f.start_vcn);
    frags.dedup_by_key(|f| f.start_vcn);
    let mut runs = Vec::new();
    let mut vcn = 0u64;
    let mut complete = true;
    for f in frags {
        if f.start_vcn > vcn {
            complete = false;
            runs.push(Run { lcn: None, len: f.start_vcn - vcn });
            vcn = f.start_vcn;
        } else if f.start_vcn < vcn {
            continue; // overlapping stale fragment
        }
        for r in f.runs {
            vcn += r.len;
            runs.push(r);
        }
    }
    (runs, complete)
}

fn runs_to_extents(runs: &[Run], cluster: u64) -> Vec<Extent> {
    runs.iter().map(|r| Extent { offset: r.lcn.map(|l| l * cluster), len: r.len * cluster }).collect()
}
