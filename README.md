# Windows Deleted Files Recovery (`wdfr`)

**Powered by Bashar Salmo**

A fast, safe, single-binary tool that recovers deleted **photos, videos, music,
documents, archives and databases** from hard drives, SSDs, USB sticks, SD
cards and disk images.

- **File-system recovery** for **NTFS**, **FAT12/16/32** and **exFAT** —
  restores original file names, folders and timestamps.
- **Signature carving** for ~50 file types — finds files even after a quick
  format, on a corrupted or unknown file system (ext4, APFS, HFS+, ...) or in
  unpartitioned space.
- **Read-only, always.** The source is never opened for writing, and the tool
  refuses to write recovered files onto the volume being recovered.
- One static executable. No installer, no runtime, nothing written to the
  damaged disk.

```text
> wdfr scan E:
== partition1_exFAT (7 deleted files) ==
CONDITION                    SIZE  MODIFIED             PATH
recoverable             114.1 KiB  2026-10-01 11:34:22  DCIM/Camera/IMG_20260101_120000.jpg
recoverable               1.9 MiB  2026-10-01 11:34:22  DCIM/Camera/VID_20260101_fragmented.mp4
recoverable             282.4 KiB  2026-10-01 11:34:22  Pictures/Trip/Day1/clip.mov
recoverable             377.9 KiB  2026-10-01 11:34:22  Pictures/Trip/scan.tif
recoverable              94.8 KiB  2026-10-01 11:34:22  Pictures/Trip/song.mp3
recoverable             232.0 KiB  2026-10-01 11:34:22  keep/contacts.sqlite
recoverable              74.7 KiB  2026-10-01 11:34:22  keep/manual.pdf
7 of 7 look recoverable.
```

---

## Before you start — read this

1. **Stop using the drive immediately.** Every file written to it (including
   browser caches and updates) can overwrite deleted data.
2. **Do not download or install anything onto that drive.** Run `wdfr` from a
   different drive or a USB stick.
3. **Recover to a different drive.** `wdfr` enforces this for volumes it can
   identify.
4. **Failing drive (clicking, very slow, read errors)?** Image it first with
   [GNU ddrescue](https://www.gnu.org/software/ddrescue/) and run `wdfr` on the
   image. `wdfr` tolerates bad sectors (it zero-fills them and keeps going),
   but every extra read stresses a dying disk.
5. **SSDs:** Windows sends TRIM when files are deleted, and the SSD may erase
   those blocks within seconds or minutes. Recovery from an SSD's own volume
   is often impossible; USB sticks, SD cards and hard drives are usually fine.

## Quick start (Windows) — no typing needed

1. Download `wdfr.exe` from the [Releases](../../releases) page and put it on
   a **different** drive than the one you want to recover.
2. Right-click `wdfr.exe` → **Run as administrator** (needed to read drives).
3. Pick everything from the menu with the arrow keys and Enter:

```text
  ================================================================
    Windows Deleted Files Recovery  v0.1.0
    Recover deleted photos, videos, documents and more
    Powered by Bashar Salmo
  ================================================================

? What would you like to do? ›
❯ Recover deleted files
  Preview deleted files (scan only, nothing is saved)
  Show drive information
  List supported file types
  Read this first: tips for a successful recovery
  Exit
```

The guide asks, one question at a time: which drive, what you are looking for
(everything, photos, videos, music, documents, or specific types), how deep to
search, and where to save the files. A folder on another drive is suggested
for you, and saving onto the drive being recovered is refused. At the end it
offers to open the folder with your files.

### Command line (advanced)

Every option is also available as a command, for scripts and power users:

```powershell
# 1. What disks and volumes are there?
wdfr devices

# 2. What is on the card / drive?
wdfr info E:

# 3. List what can be recovered (writes nothing)
wdfr scan E:

# 4. Recover everything to another drive
wdfr recover E: -o D:\Recovered
```

On macOS and Linux use the device path with `sudo`, e.g.
`sudo wdfr recover /dev/rdisk4 -o ~/Recovered` or
`sudo wdfr recover /dev/sdb -o ~/Recovered`.

Disk images (`.dd`, `.img`, `.raw`, ddrescue output) work everywhere without
admin rights: `wdfr recover card.img -o Recovered`.

## Common recipes

```powershell
# Only photos and videos
wdfr recover E: -o D:\Recovered --category image,video

# Only specific types
wdfr recover E: -o D:\Recovered --type jpg,heic,mp4,mov

# Files from a particular folder, by name pattern
wdfr scan C: --name "Users/*/Documents/**" --type docx,xlsx,pdf

# Skip tiny files (thumbnails, icons)
wdfr recover E: -o D:\Recovered --category image --min-size 100K

# Card was formatted / file system is unreadable: carve only
wdfr recover E: -o D:\Recovered --method carve

# Whole physical disk (all partitions + unpartitioned space,
# e.g. after a partition was deleted)
wdfr info \\.\PhysicalDrive1
wdfr recover \\.\PhysicalDrive1 -o D:\Recovered

# Only one partition of it
wdfr recover \\.\PhysicalDrive1 -p 2 -o D:\Recovered

# Machine-readable listing
wdfr scan E: --json > deleted.json
```

Press **Ctrl+C** once to stop gracefully (the report is still written), twice
to abort immediately.

## Output

```text
D:\Recovered\
├── partition1_NTFS\           files recovered from file-system metadata,
│   ├── Users\bob\Pictures\…   with their original folders and names
│   └── $Orphan\…              files whose parent folder no longer exists
├── carved\                    files found by signature carving
│   ├── images\f00001a2b3000.jpg
│   ├── videos\…
│   ├── audio\  documents\  archives\  databases\
└── report.csv                 one row per recovered file
```

Carved files are named after their byte offset on the disk (`f<hex offset>`),
which makes every result traceable. `report.csv` lists, for every file: how it
was recovered, its original path, the recovered path, size, disk offset,
condition, modification time, notes and the number of unreadable bytes.

Recovered files keep their original modification time.

## How it works

`wdfr recover` (default `--method all`) runs two stages:

**1. File-system metadata.** For each volume it walks the on-disk structures
and finds entries marked as deleted:

| File system | What survives deletion | What `wdfr` does |
|---|---|---|
| **NTFS** | The MFT record (name, parent, timestamps, data run list) until it is reused | Scans every MFT record (applying update-sequence fixups), rebuilds paths from parent references with sequence-number checks, follows `$ATTRIBUTE_LIST` extension records, decompresses LZNT1-compressed files, handles sparse files, resident (tiny) files and alternate data streams |
| **FAT12/16/32** | The directory entry, minus its first character; the cluster chain is erased | Reconstructs long file names (recovering the lost first character from the LFN checksum), walks into deleted folders (verifying each claimed folder cluster through its `..` back-pointer), and assigns clusters to files around other files to undo common fragmentation; restores the high word of FAT32 start clusters that Windows clears |
| **exFAT** | The whole entry set, with the "in use" bit cleared | Recovers exact names and sizes; uses the `NoFatChain` flag or the FAT chain when it survives, contiguous allocation otherwise |

Every file is checked against the volume's allocation map (`$Bitmap`, the FAT,
the exFAT bitmap): clusters that have since been reused mean the content was
overwritten. Files are labelled `recoverable`, `partial (N% intact)` or
`overwritten`; overwritten files are skipped unless you pass
`--include-overwritten`.

**2. Carving.** The unallocated space of every volume, plus any disk space not
covered by a partition, is scanned for file signatures. Space belonging to
files already recovered in stage 1 is excluded, so nothing is recovered twice
and existing files are not "recovered" (use `--carve-all-space` to scan
everything).

Rather than cutting at a fixed size or at the next footer, each format parser
**walks the file's own structure to find its exact end**: JPEG marker segments
and entropy-coded scans, PNG chunks, MP4/MOV box trees, Matroska EBML
elements, RIFF chunks (including AVI's OpenDML extensions), ASF headers,
transport-stream packets, MP3 frames, Ogg pages, ZIP central directories (with
offset consistency checks), OLE2 sector allocation tables, 7z/RAR headers with
CRC validation, and so on. The result is far fewer truncated, bloated or
broken files.

### Supported carving formats

| Category | Formats |
|---|---|
| Images | JPEG, PNG, GIF, BMP, TIFF, HEIC/HEIF, AVIF, WebP, camera RAW (CR2, CR3, NEF, ARW, DNG, PEF, SRW) |
| Video | MP4, MOV (incl. legacy QuickTime without `ftyp`), M4V, 3GP, MKV, WebM, AVI, WMV, MTS/M2TS (AVCHD), TS |
| Audio | MP3, WAV, M4A, WMA, OGG, Opus |
| Documents | PDF, DOCX, XLSX, PPTX, DOC, XLS, PPT, MSG, ODT, ODS, ODP, EPUB |
| Archives | ZIP, 7z, RAR (v4 and v5), JAR, APK |
| Databases | SQLite |

`wdfr formats` prints the current list.

## Validation

Besides its unit and integration tests, `wdfr` has been checked against:

| Test | Result |
|---|---|
| [DFTT #7 — NTFS undelete](https://dftt.sourceforge.net/) (resident, fragmented and multi-cluster files, alternate data stream, deleted and reallocated directories, leap-year dates) | **9/9** files with correct MD5, correct dates; `dir3\sing2.dat` reported under `$Orphan` because its parent's MFT entry was reallocated (as the test intends) |
| [DFTT #6 — FAT undelete](https://dftt.sourceforge.net/) | **4/6** correct MD5 (incl. one fragmented file in deleted nested directories, at its correct path). The two remaining files are deliberately interleaved cluster-by-cluster and cannot be told apart from metadata alone |
| [DFTT #11 — basic data carving](https://dftt.sourceforge.net/) | **13/15** exact MD5. Misses: a deliberately corrupted JPEG header, and a WAV whose reference file has one byte beyond its RIFF structure (the recovered audio is complete) |
| Real FAT32 (MBR) and exFAT (GPT) volumes with deleted files, deleted folder trees and a fragmented video | All files recovered byte-identical |
| 19 real files of different formats embedded in random data | 19/19 byte-identical, no false positives |
| 1 GiB of random data | 0 false positives |

## Limitations

- **SSDs with TRIM** usually erase deleted data — nothing can recover what the
  drive has wiped.
- **Encryption:** for BitLocker, recover from the *unlocked* volume
  (`\\.\C:`), not the physical disk. EFS-encrypted files are recovered as
  ciphertext (flagged in the report).
- **Fragmentation:** carving assumes a file is stored contiguously (true for
  most camera and phone media). FAT fragment reconstruction is a best guess
  and is labelled as such.
- **Other file systems** (ext4, APFS, HFS+, Btrfs, ReFS) are supported by
  carving only.
- `--deep` (byte-granular carving) is CPU-bound (~150 MB/s); the default
  sector-aligned scan runs at disk speed.

## Building from source

Requires [Rust](https://rustup.rs) 1.88 or newer.

```sh
cargo build --release          # target/release/wdfr(.exe)
cargo test                     # unit + integration tests
```

The Windows release binary is built with a statically linked C runtime (see
`.cargo/config.toml`), so it runs on a bare Windows install.

### Code layout

| Module | Responsibility |
|---|---|
| `source` | Read-only device/image access: sector alignment for raw devices, bad-sector tolerant reads |
| `partition` | MBR (incl. extended/logical) and GPT discovery, file-system detection |
| `fs::ntfs` | MFT parsing, fixups, run lists, attribute lists, LZNT1 |
| `fs::fat`, `fs::exfat` | Directory walking, LFN recovery, cluster assignment |
| `carve` | Scanner and per-format structure parsers |
| `recover` | Orchestration, de-duplication between stages, progress, report |
| `output` | Safe naming (recovered names are untrusted input and can never escape the output folder) |

## License

[MIT](LICENSE) — Powered by Bashar Salmo
