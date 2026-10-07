//! `wdfr` — deleted file recovery for NTFS, FAT12/16/32 and exFAT volumes,
//! with signature-based file carving as a fallback for everything else.
//!
//! The crate is organised in layers:
//!
//! * [`source`] — read-only, sector-aligned, bad-sector tolerant access to
//!   raw devices and disk images.
//! * [`partition`] — MBR / GPT discovery and file-system detection.
//! * [`fs`] — file-system parsers that find deleted entries in metadata
//!   (MFT records, directory entries) and know where their data lived.
//! * [`carve`] — file-system independent carving: finds files by their
//!   signatures and computes their exact length from the format structure.
//! * [`recover`] — orchestration: filtering, extraction and reporting.

pub mod bytes;
pub mod carve;
pub mod dedupe;
pub mod devices;
pub mod filter;
pub mod fragments;
pub mod fs;
pub mod imaging;
pub mod output;
pub mod partition;
pub mod progress;
pub mod ranges;
pub mod recover;
pub mod rescue;
pub mod saved;
pub mod source;
pub mod units;
pub mod verify;
