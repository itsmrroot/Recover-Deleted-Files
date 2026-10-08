<div align="center">

<img src="assets/banner.svg" alt="Deleted Files Recovery — Powered by Bashar Salmo" width="100%">

<br>

[![CI](https://github.com/itsmrroot/Recover-Deleted-Files/actions/workflows/ci.yml/badge.svg)](https://github.com/itsmrroot/Recover-Deleted-Files/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/itsmrroot/Recover-Deleted-Files?color=0b5cad)](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/itsmrroot/Recover-Deleted-Files/total?color=16a34a)](https://github.com/itsmrroot/Recover-Deleted-Files/releases)
[![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-6b7280)](#-install)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-dea584?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/github/license/itsmrroot/Recover-Deleted-Files?color=a78bfa)](LICENSE)

**Bring back deleted photos, videos, music and documents —<br>from hard drives, USB sticks, SD cards and disk images.**

[![Install on Windows](https://img.shields.io/badge/Install%20on%20Windows-0078D6?style=for-the-badge&logo=windows&logoColor=white)](#-windows)
[![Install on macOS](https://img.shields.io/badge/macOS-000000?style=for-the-badge&logo=apple&logoColor=white)](#-macos)
[![Install on Linux](https://img.shields.io/badge/Linux-FCC624?style=for-the-badge&logo=linux&logoColor=black)](#-linux)

[Quick start](#-quick-start) •
[Install](#-install) •
[Desktop app](#%EF%B8%8F-the-desktop-app) •
[Features](#-features) •
[Command line](#-command-line) •
[How it works](#%EF%B8%8F-how-it-works) •
[File types](#%EF%B8%8F-supported-file-types) •
[FAQ](#-faq)

</div>

---

## ✨ Features

<table>
<tr>
<td width="50%" valign="top">

### 🖥️ Modern desktop app
Pick a drive, scan, preview photos, tick the files you want and click
**Recover**. In English, German, Spanish, French, Turkish, Russian, Arabic and
Chinese, with Midnight (the default), dark and light themes, accent colours and saved settings.
A built-in Help page walks through common situations step by step.
The app tells you when a new version is out and installs it in one click.

</td>
<td width="50%" valign="top">

### 📁 Original names & folders
On **NTFS**, **FAT32** and **exFAT** (Windows, cards, sticks), **APFS** and
**Mac OS Extended** (Mac) and **ext2/3/4** (Linux), deleted files come back with
their real names, folders and dates.

</td>
</tr>
<tr>
<td valign="top">

### 🔍 Deep search
Formatted card? Corrupted drive? Files are found by their content in **~60 file
types**, and each is cut at its **exact** length — no broken or bloated files.
Videos a camera stored **in pieces** are put back together, frame by frame.

</td>
<td valign="top">

### 🛡️ Safe by design
The drive is opened **read-only**, and saving onto the drive you are recovering
is refused. Bad sectors are skipped, not fatal. A failing drive can be **copied
to an image** first — read once, damaged areas last, resumable — and the copy
scanned instead.

</td>
</tr>
<tr>
<td valign="top">

### 🩺 Honest results
Every file is checked against the drive's allocation map and marked
**recoverable**, **partial** or **overwritten** — no guessing. Files an SSD has
already wiped (TRIM) are detected and marked **erased** instead of promising
files full of zeros.

</td>
<td valign="top">

### ⚡ Fast & portable
Installers for Windows, macOS and Linux, or portable single executables with no
runtime — nothing is written to the damaged drive. Scans at full disk speed.

</td>
</tr>
<tr>
<td valign="top">

### 🏷️ Real names, even without a file system
Files from an emptied **Recycle Bin** get their original name and folder back.
Files found by their content are named from what is inside them —
`2024-08-21 18.45.03 iPhone 15 Pro.jpg`, `Artist - Title.mp3`, a PDF's title —
and dated, so they can be filtered by year.

</td>
<td valign="top">

### 🧹 No clutter, no waiting
Identical copies are found (confirmed byte by byte) and hidden. Browse and preview
files **while the scan is still running**, and save a scan to reopen it later
without scanning again.

</td>
</tr>
<tr>
<td valign="top">

### 🧭 Lost partitions & formatted drives
The deep search also finds **deleted partitions** and the old file table of a
**quick-formatted NTFS drive** — those files come back with their **names and
folders**, not just their content.

</td>
<td valign="top">

### ✅ Verified before you save
Every file found is checked in the background: complete files are marked
**Verified**, files with missing or broken parts **May be damaged** — photos are
fully decoded. Show only verified files with one click.

</td>
</tr>
<tr>
<td valign="top">

### 🔓 BitLocker drives
A drive locked with **BitLocker** opens with its 48-digit **recovery key** or
its password — XTS, CBC and Windows 7's Elephant, USB sticks (To Go) too. It is
decrypted as it is read: nothing on it changes.

</td>
<td valign="top">

### 🕰️ Older copies
Files in Windows **Previous Versions** (System Restore points) and in **APFS
snapshots** (Time Machine) come back even after their space was reused. A
damaged APFS drive is rebuilt from what is left of its file tables.

</td>
</tr>
<tr>
<td valign="top">

### 🔐 Private by choice
Tick **Protect with a password** and everything is saved into one **AES-256
encrypted ZIP** — nothing is written unencrypted. It opens in 7-Zip, WinRAR
or Keka.

</td>
<td valign="top">

### 🩹 Photo repair & folder scans
**Repair** a damaged JPEG: close a cut-off photo, or give it the header of a good
photo from the same camera. Look **only in one folder** (e.g.
`Users/Ann/Pictures`) for a short list.

</td>
</tr>
</table>

## 🚀 Quick start

> [!IMPORTANT]
> **Stop using the drive right away.** Every new file saved to it can overwrite the files you want back.

1. **Install** the app for your system: **[Windows](#-windows)** · **[macOS](#-macos)** · **[Linux](#-linux)**
   (on a **different** drive than the one you want to recover).
2. Open **Deleted Files Recovery**. Windows asks for administrator rights: click **Yes**. On macOS, click
   **Allow access to drives**, enter your password and give the app [Full Disk Access](#-macos). On Linux,
   click **Restart with administrator rights** and enter your password.
3. Choose the drive, click **Start scan**, tick the files you want and click **Recover**.

<div align="center">
<img src="assets/app-results.png" alt="The wdfr desktop app showing deleted files found on a drive, with a photo preview" width="100%">
</div>

> [!TIP]
> Prefer the keyboard? **`wdfr`** (included in every download) opens a step-by-step text menu, and offers every
> option on the command line.

## 🖥️ The desktop app

| | |
|---|---|
| **1 · Choose** | Every drive is listed as Windows names it (*USB Drive (E:)*, *Local Disk (C:)*) with its size and file system. Disk images can be opened or simply dragged onto the window. Pick what you are looking for — photos, videos, audio, documents… — and how deep to search. |
| **2 · Scan** | Live progress with speed, time left and a running count of what was found, by type. Stop at any time: everything found so far is kept. |
| **3 · Choose files** | A fast, searchable, sortable list of everything found — even hundreds of thousands of files. Filter by type or by how each file was found, preview photos and text files, and see whether each file is *recoverable*, *partial* or *overwritten* (and why). |
| **4 · Recover** | A folder on another drive is suggested automatically; saving onto the drive being recovered is refused. When done, the folder opens with your files and a report. |

<table>
<tr>
<td width="50%"><img src="assets/app-scanning.png" alt="Scanning a drive"><p align="center"><sub>Scanning</sub></p></td>
<td width="50%"><img src="assets/app-settings.png" alt="Settings"><p align="center"><sub>Settings</sub></p></td>
</tr>
<tr>
<td colspan="2"><img src="assets/app-results-light.png" alt="Results in light mode"><p align="center"><sub>Light mode</sub></p></td>
</tr>
</table>

**Settings** (saved automatically): language (system / English / Deutsch / Español / Français / Türkçe /
Русский / العربية / 简体中文), theme
(Midnight / dark / light / system), accent colour, checking for updates at start, interface size, default
search mode, minimum file size, whole-drive and byte-level deep search, largest file size, showing whole
disks, default destination, folder layout (*original folders* or *sorted by type*), restoring original
dates, the CSV report, opening the folder when done, showing overwritten files — and, clearly marked as
unsafe, allowing saves to the source drive.

## 📥 Install

Click your system to see which file to download and how to install it:

<div align="center">

[![Windows](https://img.shields.io/badge/Windows-0078D6?style=for-the-badge&logo=windows&logoColor=white)](#-windows)
[![macOS](https://img.shields.io/badge/macOS-000000?style=for-the-badge&logo=apple&logoColor=white)](#-macos)
[![Linux](https://img.shields.io/badge/Linux-FCC624?style=for-the-badge&logo=linux&logoColor=black)](#-linux)
[![Command line](https://img.shields.io/badge/Command%20line-4b5563?style=for-the-badge&logo=gnubash&logoColor=white)](#-command-line-only)
[![Portable](https://img.shields.io/badge/Portable%20(no%20install)-6b7280?style=for-the-badge&logo=files&logoColor=white)](#-portable-no-installation)

</div>

Every installer contains the desktop app **and** the command line (`wdfr`). All files are also on the
**[latest release](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest)** page.

> [!IMPORTANT]
> Download and install the app on a **different drive** than the one you want to recover, so that nothing
> overwrites the deleted files.

### 🪟 Windows

**1. Download** the installer for your PC:

| Your PC | Download |
|---|---|
| **Most PCs** (Intel or AMD) | **[wdfr-windows-x64-setup.exe](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-windows-x64-setup.exe)** |
| ARM laptops (Snapdragon, Surface Pro X) | [wdfr-windows-arm64-setup.exe](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-windows-arm64-setup.exe) |

<sub>Not sure? **Settings → System → About → System type** says *x64-based* or *ARM-based*.</sub>

**2. Install**

1. Double-click the downloaded file.
2. If Windows shows **"Windows protected your PC"**, click **More info → Run anyway**. (The app is new and
   not code-signed yet.)
3. Pick your language, click **Next → Install** and **Yes** when Windows asks for permission.

**3. Start**: open the **Start menu** → **Deleted Files Recovery**, and click **Yes** when Windows asks for
administrator rights (needed to read drives).

<sub>**Uninstall:** Settings → Apps → Installed apps → *Deleted Files Recovery* → Uninstall.</sub>

### 🍎 macOS

**1. Download** the disk image for your Mac:

| Your Mac | Download |
|---|---|
| **Apple Silicon** (M1, M2, M3, M4 and newer) | **[wdfr-macos-apple-silicon.dmg](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-macos-apple-silicon.dmg)** |
| Intel | [wdfr-macos-intel.dmg](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-macos-intel.dmg) |

<sub>Not sure? **Apple menu  → About This Mac** shows *Chip: Apple M…* or *Processor: Intel*.</sub>

**2. Install**

1. Open the downloaded `.dmg`.
2. Drag **Deleted Files Recovery** onto the **Applications** folder.
3. The first time, macOS blocks the app because it is not signed by Apple yet:
   - **macOS 15 Sequoia and newer:** open the app, click **Done**, then go to **System Settings → Privacy &
     Security**, scroll down and click **Open Anyway**.
   - **macOS 14 and older:** in Applications, **right-click** (or Control-click) the app → **Open** → **Open**.

   You only need to do this once.

**3. Start**: open **Deleted Files Recovery** from Applications or Launchpad. To read drives:

1. Click **Allow access to drives** and enter your Mac password.
2. macOS also needs **Full Disk Access**, even for administrators. Click **Open Full Disk Access settings**
   (or go to **System Settings → Privacy & Security → Full Disk Access**), turn on **Deleted Files
   Recovery** (use **+** if it is not listed), then click **Refresh** in the app.

Disk images work without either step.

> [!NOTE]
> On a Mac, the app is most useful for **USB sticks, SD cards and external drives** (FAT32, exFAT, NTFS, APFS,
> Mac OS Extended). The Mac's own drive is encrypted and its SSD erases deleted data on its own, so little can be
> recovered from it.

<sub>**Uninstall:** drag *Deleted Files Recovery* from Applications to the Trash.</sub>

### 🐧 Linux

**1. Download** the package for your distribution and processor:

| Your distribution | Intel / AMD (`x86_64`) | ARM (`aarch64`) |
|---|---|---|
| **Ubuntu, Debian, Mint, Pop!_OS** | **[wdfr-linux-x86_64.deb](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-linux-x86_64.deb)** | [wdfr-linux-arm64.deb](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-linux-arm64.deb) |
| **Fedora, openSUSE, RHEL** | [wdfr-linux-x86_64.rpm](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-linux-x86_64.rpm) | [wdfr-linux-arm64.rpm](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-linux-arm64.rpm) |
| **Any other** (AppImage) | [wdfr-linux-x86_64.AppImage](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-linux-x86_64.AppImage) | [wdfr-linux-arm64.AppImage](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest/download/wdfr-linux-arm64.AppImage) |

<sub>Not sure? Run `uname -m` in a terminal: `x86_64` is Intel/AMD, `aarch64` is ARM (Raspberry Pi 4/5 with
a 64-bit system, ARM servers, Linux virtual machines on Apple Silicon Macs). The app needs Ubuntu 22.04,
Debian 12, Fedora 36, Mint 21 or newer.</sub>

**2. Install** (in a terminal, in the folder you downloaded to, e.g. `cd ~/Downloads`). Use the file name you
downloaded; the examples show Intel/AMD.

<details open>
<summary><b>Ubuntu, Debian, Mint (.deb)</b></summary>

```bash
sudo apt install ./wdfr-linux-x86_64.deb
```

</details>

<details>
<summary><b>Fedora, openSUSE, RHEL (.rpm)</b></summary>

```bash
sudo dnf install ./wdfr-linux-x86_64.rpm       # Fedora, RHEL
sudo zypper install ./wdfr-linux-x86_64.rpm    # openSUSE
```

</details>

<details>
<summary><b>Any distribution (AppImage, no installation)</b></summary>

```bash
chmod +x wdfr-linux-x86_64.AppImage
./wdfr-linux-x86_64.AppImage
```

If it complains about `libfuse.so.2`, install FUSE 2 (`sudo apt install libfuse2` on Ubuntu 22.04,
`libfuse2t64` on 24.04) or start it with `--appimage-extract-and-run`.

</details>

**3. Start**: open **Deleted Files Recovery** from your app menu (or run `wdfr-gui` in a terminal). To read
drives, click **Restart with administrator rights** and enter your password. Disk images work without it.

<details>
<summary><b>The app does not open</b></summary>

Run `wdfr-gui` in a terminal to see why:

- **`GLIBC_2.xx not found`**: your distribution is older than the versions above. Use the
  [command line](#-command-line-only) instead, which runs everywhere.
- **Nothing appears in a virtual machine**: turn on **3D acceleration** in the VM's display settings. The app
  draws its window with your graphics card (Vulkan or OpenGL).

</details>

<sub>**Uninstall:** `sudo apt remove deleted-files-recovery` (.deb), `sudo dnf remove wdfr` (.rpm), or delete
the AppImage.</sub>

### 💻 Command line only

The command line runs on any Linux, with no dependencies, and is also included in every installer and
portable download. Download it from the [latest release](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest):

| Your computer | File |
|---|---|
| Linux, Intel/AMD | `wdfr-…-x86_64-unknown-linux-musl.tar.gz` |
| Linux, ARM | `wdfr-…-aarch64-unknown-linux-musl.tar.gz` |

```bash
tar xzf wdfr-*-linux-musl.tar.gz
sudo ./wdfr-*/wdfr          # step-by-step menu; see "Command line" below for all commands
```

### 📦 Portable (no installation)

Unpack and run, nothing is installed. All files are on the
[latest release](https://github.com/itsmrroot/Recover-Deleted-Files/releases/latest) page:

| Your computer | File | Contains |
|---|---|---|
| Windows (Intel/AMD) | `wdfr-…-x86_64-pc-windows-msvc.zip` | desktop app + command line |
| Windows on ARM | `wdfr-…-aarch64-pc-windows-msvc.zip` | desktop app + command line |
| Mac with Apple Silicon | `wdfr-…-aarch64-apple-darwin.tar.gz` | desktop app + command line |
| Mac with Intel | `wdfr-…-x86_64-apple-darwin.tar.gz` | desktop app + command line |
| Linux, Intel/AMD | `wdfr-…-x86_64-unknown-linux-gnu-desktop.tar.gz` | desktop app |
| Linux, ARM | `wdfr-…-aarch64-unknown-linux-gnu-desktop.tar.gz` | desktop app |

On macOS and Linux, start the portable app with `sudo ./wdfr-gui` to read drives; disk images work without `sudo`.
On macOS, the Terminal app also needs Full Disk Access (System Settings → Privacy & Security).

## 💻 Command line

`wdfr` (no arguments) opens a step-by-step text menu. Everything is also available as a command — handy for
scripts and power users.

```powershell
wdfr devices                         # list drives
wdfr info E:                         # partitions and file systems on E:
wdfr scan E:                         # list deleted files (writes nothing)
wdfr recover E: -o D:\Recovered      # recover everything to another drive
```

<details>
<summary><b>More recipes</b></summary>

```powershell
# Only photos and videos
wdfr recover E: -o D:\Recovered --category image,video

# Only specific types
wdfr recover E: -o D:\Recovered --type jpg,heic,mp4,mov

# Files from a particular folder, by name pattern
wdfr scan C: --name "Users/*/Documents/**" --type docx,xlsx,pdf

# Skip tiny files (thumbnails, icons)
wdfr recover E: -o D:\Recovered --category image --min-size 100K

# Card was formatted / file system is unreadable: search by content only
wdfr recover E: -o D:\Recovered --method carve

# Whole physical disk (all partitions + unpartitioned space,
# e.g. after a partition was deleted)
wdfr info \\.\PhysicalDrive1
wdfr recover \\.\PhysicalDrive1 -o D:\Recovered

# Only one partition of it
wdfr recover \\.\PhysicalDrive1 -p 2 -o D:\Recovered

# Only files dated in 2024 (carved photos are dated from their EXIF data)
wdfr recover E: -o D:\Recovered --after 2024-01-01 --before 2024-12-31

# Also write identical copies (skipped by default)
wdfr recover E: -o D:\Recovered --keep-duplicates

# Only files that were in one folder (or below it)
wdfr scan E: --folder "Users/Ann/Pictures"

# A drive locked with BitLocker: give its recovery key (or password)
wdfr recover \\.\PhysicalDrive1 -o D:\Recovered --bitlocker-key 123456-123456-...

# Save everything into one AES-256 encrypted ZIP, D:\Recovered.zip
wdfr recover E: -o D:\Recovered --password "correct horse battery staple"

# Repair a damaged photo (with a good one from the same camera if its start is gone)
wdfr repair f00001a2b3000.jpg --reference good.jpg -o repaired.jpg

# Machine-readable listing
wdfr scan E: --json > deleted.json

# A failing drive: copy it once (damaged areas last), then work on the copy.
# Run the same command again to continue an interrupted copy.
wdfr image \\.\PhysicalDrive1 D:\drive.img
wdfr recover D:\drive.img -o D:\Recovered
```

On macOS and Linux use the device path with `sudo`, e.g. `sudo wdfr recover /dev/rdisk4 -o ~/Recovered`
(on macOS, give your terminal app Full Disk Access in System Settings → Privacy & Security first).
Disk images (`.dd`, `.img`, `.raw`, ddrescue output) work everywhere without admin rights:
`wdfr recover card.img -o Recovered`.

Press **Ctrl+C** once to stop gracefully (everything found so far is kept), twice to abort immediately.

</details>

<details>
<summary><b>What you get in the output folder</b></summary>

```text
D:\Recovered\
├── partition1_NTFS\           files recovered with their names,
│   ├── Users\bob\Pictures\…   in their original folders
│   └── $Orphan\…              files whose parent folder no longer exists
├── found1_NTFS\               a lost partition or the old file table of a formatted
│   └── Photos\…               drive, found by the deep search: with names and folders
├── carved\                    files found by deep search, sorted by type
│   ├── images\f00001a2b3000.jpg
│   ├── videos\…
│   └── audio\  documents\  archives\  databases\
└── report.csv                 one row per recovered file
```

Deep-search files are named after their position on the disk (`f<hex offset>`), so every result is traceable.
`report.csv` lists how each file was recovered, its original and new path, size, disk offset, condition,
modification time and notes. Recovered files keep their original modification time.

</details>

## ⚙️ How it works

<img src="assets/how-it-works.svg" alt="How wdfr works: stage 1 restores deleted files from file-system metadata, stage 2 deep-searches the remaining free space" width="100%">

**Stage 1 — file-system metadata.** Deleted files usually leave their entry behind. `wdfr` reads it to restore the
name, folder, dates and the location of the data, then checks whether those clusters were reused since.

**Stage 2 — deep search (carving).** The free space of every volume and any unpartitioned space is scanned for file
signatures. Rather than cutting at a fixed size, each format parser walks the file's own structure — JPEG segments,
MP4 boxes, ZIP central directory, PDF trailers, … — to find exactly where it ends. Space already restored in stage 1 is
skipped, so nothing is recovered twice and existing files are left out.

**Also in stage 2 — lost partitions and old file tables.** The same pass looks for boot sectors and NTFS file records
in the space it reads. A boot sector (or its backup copy) that no current partition starts with is opened as a **lost
partition**, and all its files are listed with their names. The MFT records a quick format left behind are rebuilt into
folders; the old volume's start and cluster size are worked out by checking candidate layouts against the files' own
content (a `.jpg` record must point at JPEG data).

**Then — checked.** Each file found is followed through its own structure from start to end, with the same parsers; photos
are also decoded. Complete files are marked **Verified**, broken or cut-short ones **May be damaged**.

<details>
<summary><b>Technical details per file system</b></summary>

| File system | What survives deletion | What `wdfr` does |
|---|---|---|
| **NTFS** | The MFT record (name, parent, timestamps, data run list) until it is reused | Scans every MFT record (applying update-sequence fixups), rebuilds paths from parent references with sequence-number checks, follows `$ATTRIBUTE_LIST` extension records, decompresses LZNT1-compressed files, handles sparse files, resident (tiny) files and alternate data streams |
| **FAT12/16/32** | The directory entry, minus its first character; the cluster chain is erased | Reconstructs long file names (recovering the lost first character from the LFN checksum), walks into deleted folders (verifying each claimed folder cluster through its `..` back-pointer), and assigns clusters to files around other files to undo common fragmentation; restores the high word of FAT32 start clusters that Windows clears |
| **exFAT** | The whole entry set, with the "in use" bit cleared | Recovers exact names and sizes; uses the `NoFatChain` flag or the FAT chain when it survives, contiguous allocation otherwise |
| **APFS** | Nothing is changed in place: older checkpoints and snapshots still describe the file tree as it was | Reads every checkpoint in the container's ring and every snapshot; files in an earlier tree but not the newest one are the deleted files, with names, folders and extents. When the container's own structures are damaged, the file tree is rebuilt from every checksummed leaf node left on the disk (encrypted volumes cannot be read) |
| **Mac OS Extended (HFS+)** | Old copies of catalog records, in freed catalog nodes and in the journal | Parses every catalog leaf node (live, freed and journaled); file records the live tree no longer has are the deleted files; follows the extents overflow file |
| **ext2/3/4** | The name in its directory (until the kernel wipes it) and older inode copies in the journal | Walks directories, reads deleted names from the gaps between entries, and takes a deleted file's block list from the newest journal copy of its inode (as extundelete does) |

**BitLocker** volumes are read through a decrypting layer: the volume master key is unwrapped (AES-CCM) with the
recovery key or password (BitLocker's SHA-256 key stretching), or directly when BitLocker is suspended; then each
sector is decrypted with AES-XTS, AES-CBC or AES-CBC + Elephant diffuser, with the moved boot sectors and the metadata
areas mapped as cryptsetup does. On Windows, each **shadow copy** of the drive is opened as a volume of its own; files
it has that the drive no longer has are listed with the date of the copy.

Carving parsers: JPEG marker segments and entropy-coded scans, PNG chunks, MP4/MOV box trees, Matroska EBML elements,
RIFF chunks (incl. AVI OpenDML), ASF headers, transport-stream packets, MP3 frames, Ogg pages, ZIP central directories
with offset consistency checks, OLE2 sector allocation tables, and 7z/RAR headers with CRC validation.

</details>

## 🗂️ Supported file types

| | Formats |
|---|---|
| 🖼️ **Images** | JPEG · PNG · GIF · BMP · TIFF · HEIC/HEIF · AVIF · WebP · Photoshop PSD/PSB · camera RAW (CR2 · CR3 · NEF · ARW · DNG · PEF · SRW · ORF · RW2 · RAF) |
| 🎬 **Video** | MP4 · MOV · M4V · 3GP · MKV · WebM · AVI · WMV · MTS/M2TS (AVCHD) · TS |
| 🎵 **Audio** | MP3 · WAV · FLAC · M4A · WMA · OGG · Opus |
| 📄 **Documents** | PDF · DOCX · XLSX · PPTX · DOC · XLS · PPT · RTF · MSG · Outlook PST/OST · ODT · ODS · ODP · EPUB |
| 🗜️ **Archives** | ZIP · 7z · RAR (v4 & v5) · JAR · APK |
| 🗄️ **Databases** | SQLite |

Files recovered through the file system (stage 1) can be **any** type — the list above applies to the deep search.

## ✅ Tested on real data

Besides 100+ unit and integration tests (including the full scan-and-save pipeline) running on Windows, macOS and Linux for every change, `wdfr` was checked against
the public [Digital Forensics Tool Testing](https://dftt.sourceforge.net/) images and real volumes:

| Test | Result |
|---|---|
| DFTT #7 — NTFS undelete | **9 / 9** files with correct MD5 and dates (incl. fragmented files, deleted folders and an alternate data stream) |
| DFTT #11 — deep search | **13 / 15** exact MD5 — misses are a deliberately corrupted JPEG and a WAV with one extra byte outside its structure |
| DFTT #6 — FAT undelete | **4 / 6** correct MD5 — the other two are deliberately interleaved cluster by cluster, which no metadata can untangle |
| Real FAT32 and exFAT volumes | **All** deleted files byte-identical, incl. a fragmented video and whole deleted folder trees |
| 19 real files hidden in random data | **19 / 19** byte-identical, no false results |
| 1 GB of random data | **0** false results |
| ext3 / ext4 made and deleted by Linux | **All** deleted files byte-identical, with names and folders (from the journal) |
| APFS / Mac OS Extended made and deleted by macOS | **All** deleted files byte-identical, with names and folders |
| A video in 13 pieces, the last before the first on the disk | Put back together **byte-identical** |
| cryptsetup's BitLocker test volumes (XTS, CBC, Elephant, To Go, 4K sectors, suspended) | Decrypted **byte-identical** to cryptsetup |
| An APFS drive with its first blocks wiped | All files back with names and folders |
| A JPEG whose header was overwritten, given another photo from the same camera | Repaired **byte-identical** |

## ❓ FAQ

<details>
<summary><b>Can it recover files from an SSD?</b></summary>

Often not. Windows tells the SSD which blocks were freed (TRIM), and the SSD may erase them within seconds or minutes.
Nothing can recover data the drive itself has wiped. The app detects this and marks such files **erased by the drive**
instead of listing them as recoverable. USB sticks, SD cards and hard drives usually recover well.

</details>

<details>
<summary><b>Why is a file marked "overwritten"?</b></summary>

Its space on the drive has since been used by another file, so the original content is gone. Such files are skipped by
default; `--include-overwritten` writes them anyway (usually as garbage).

</details>

<details>
<summary><b>Why do some files have names like <code>_mage.jpg</code> or <code>f0001a2b3000.jpg</code>?</b></summary>

FAT/exFAT deletion destroys the first character of short names, so it is replaced by `_`. Files found by the deep
search have no name left at all; they are named after their position on the disk.

</details>

<details>
<summary><b>The drive makes noises or is very slow.</b></summary>

Copy it to an image file first, then scan the image: every extra read stresses a dying drive. In the app, select the
drive and click **Copy to an image…**; on the command line, `wdfr image <drive> <file.img>`. Like
[GNU ddrescue](https://www.gnu.org/software/ddrescue/) (whose map files it reads and writes), it copies what reads
easily first, retries damaged areas last, and continues an interrupted copy.

</details>

<details>
<summary><b>The drive was formatted, or a partition was deleted.</b></summary>

Use **Recommended** or **Formatted drive**. Besides finding files by their content, the deep search finds deleted
partitions and the old file table of a quick-formatted NTFS drive, so those files come back with their names and
folders. A full format (which overwrites the drive) or a drive that was used a lot since leaves less behind.

</details>

<details>
<summary><b>Does it work with BitLocker or EFS?</b></summary>

Yes. A BitLocker drive that Windows has unlocked (e.g. `E:`) is read like any other. A locked one — or one from
another computer, or on a Mac or Linux — opens with its 48-digit recovery key or its password: select it and click
**Unlock** in the app, or use `--bitlocker-key` on the command line. A suspended one opens without a key. Nothing
is written to the drive and the key is not stored. EFS-encrypted files are recovered as ciphertext and flagged in
the report.

</details>

<details>
<summary><b>What about Mac and Linux drives (APFS, HFS+, ext4) or other file systems?</b></summary>

APFS, Mac OS Extended (HFS+) and ext2/3/4 are read with names and folders, including deleted files where their
records survive (see *How it works*). Other file systems are supported by the deep search, without original names.

</details>

## 🛠️ Build from source

Requires [Rust](https://rustup.rs) 1.88 or newer.

```sh
cargo build --release --workspace   # → target/release/wdfr(.exe) and wdfr-gui(.exe)
cargo test --workspace              # unit + integration tests
```

Release builds for every platform are produced automatically when a version tag (`v*`) is pushed. The Windows binary
links the C runtime statically, so it runs on a bare Windows install.

<details>
<summary><b>Code layout</b></summary>

| Module | Responsibility |
|---|---|
| `gui/` | The desktop app (egui): screens, theme, settings, background jobs, previews |
| `menu` | Interactive text menu of the command line |
| `progress` | Progress reporting shared by the command line and the desktop app |
| `source` | Read-only device/image access: sector alignment for raw devices, bad-sector tolerant reads |
| `partition` | MBR (incl. extended/logical) and GPT discovery, file-system detection |
| `fs::ntfs` | MFT parsing, fixups, run lists, attribute lists, LZNT1 |
| `fs::fat`, `fs::exfat` | Directory walking, long-name recovery, cluster assignment |
| `carve` | Deep-search scanner and per-format structure parsers |
| `recover` | Scan / save orchestration, de-duplication between stages, report |
| `output` | Safe naming — recovered names are untrusted and can never escape the output folder |

</details>

---

<div align="center">

**Powered by Bashar Salmo**

Released under the [MIT License](LICENSE). The desktop app bundles
[Noto Sans Arabic](https://github.com/notofonts/arabic) and [Noto Sans SC](https://github.com/notofonts/noto-cjk)
under the SIL Open Font License ([Arabic](gui/assets/fonts/OFL.txt), [SC](gui/assets/fonts/OFL-NotoSansSC.txt)).

</div>
