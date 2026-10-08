//! Reading BitLocker drives with their recovery key or password.
//!
//! A BitLocker volume is unreadable without its key, and so are its deleted
//! files. With the 48-digit recovery key (or the password, or none at all
//! when BitLocker is suspended) the volume key is unwrapped from the FVE
//! metadata, and the volume is read through [`Unlocked`], which decrypts
//! each sector on the fly — the drive itself is never changed.
//!
//! Layout (as cryptsetup's `bitlk` reads it):
//! * three copies of the FVE metadata (64 KiB each), at offsets given in the
//!   boot sector, read as zeros;
//! * the original first sectors of the volume (its real boot sector), moved
//!   elsewhere and encrypted there;
//! * everything else encrypted in place, sector by sector: AES-XTS (Windows
//!   10 and later), AES-CBC, or AES-CBC with the Elephant diffuser (Windows
//!   7). With "used space only" encryption, never-used sectors stay blank,
//!   and are read as blank.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Aes256};
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

use crate::bytes::{le16, le32, le64};
use crate::source::{ReadAt, Source};

const SIGNATURE: &[u8; 8] = b"-FVE-FS-";
const SIGNATURE_TOGO: &[u8; 8] = b"MSWIN4.1";
const METADATA_SIZE: u64 = 64 * 1024;
/// Type GUIDs of the BitLocker "superblock" (normal, encrypt-on-write).
const GUID_NORMAL: [u8; 16] =
    [0x3b, 0xd6, 0x67, 0x49, 0x29, 0x2e, 0xd8, 0x4a, 0x83, 0x99, 0xf6, 0xa3, 0x39, 0xe3, 0xd0, 0x01];
const GUID_EOW: [u8; 16] =
    [0x3b, 0x4d, 0xa8, 0x92, 0x80, 0xdd, 0x0e, 0x4d, 0x9e, 0x4e, 0xb1, 0xe3, 0x28, 0x4e, 0xae, 0xd8];
const KDF_ITERATIONS: u64 = 0x10_0000;

// VMK protectors.
const PROTECTION_CLEAR_KEY: u16 = 0x0000;
const PROTECTION_RECOVERY_PASSWORD: u16 = 0x0800;
const PROTECTION_PASSWORD: u16 = 0x2000;

/// Where the FVE metadata copies are, if `src` is a BitLocker volume.
fn metadata_offsets(src: &dyn ReadAt) -> Option<[u64; 3]> {
    let mut bs = [0u8; 512];
    src.read_exact_at(0, &mut bs).ok()?;
    let at = if &bs[3..11] == SIGNATURE {
        160
    } else if &bs[3..11] == SIGNATURE_TOGO {
        // BitLocker To Go: a FAT boot sector that only shows a reader app.
        424
    } else {
        return None;
    };
    let guid = &bs[at..at + 16];
    if guid != GUID_NORMAL && guid != GUID_EOW {
        return None;
    }
    let o = [le64(&bs, at + 16)?, le64(&bs, at + 24)?, le64(&bs, at + 32)?];
    Some(o)
}

/// Whether `src` is a BitLocker volume.
pub fn is_bitlocker(src: &dyn ReadAt) -> bool {
    metadata_offsets(src).is_some()
}

/// A key wrapped with AES-CCM.
#[derive(Debug, Clone, Default)]
struct Wrapped {
    nonce: [u8; 12],
    tag: [u8; 16],
    data: Vec<u8>,
}

impl Wrapped {
    fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 28 {
            return None;
        }
        Some(Self { nonce: d[..12].try_into().ok()?, tag: d[12..28].try_into().ok()?, data: d[28..].to_vec() })
    }

    /// The key inside (after its 12-byte header), if `key` is right.
    fn unwrap(&self, key: &[u8]) -> Option<Vec<u8>> {
        let plain = ccm_decrypt(&Aes::new(key)?, &self.nonce, &self.tag, &self.data)?;
        // u16 size, u16 role, u16 type, u16 flags, u32 encryption method
        if usize::from(le16(&plain, 0)?) != plain.len() || plain.len() < 12 {
            return None;
        }
        Some(plain[12..].to_vec())
    }
}

#[derive(Debug, Clone, Default)]
struct Vmk {
    protection: u16,
    salt: [u8; 16],
    wrapped: Option<Wrapped>,
    /// The key that opens `wrapped`, stored in the clear (BitLocker suspended).
    clear_key: Option<Vec<u8>>,
}

/// What the FVE metadata says.
#[derive(Debug, Clone)]
struct Metadata {
    guid: [u8; 16],
    sector_size: u64,
    /// The bytes after this are not encrypted (yet).
    encrypted_size: u64,
    method: u16,
    offsets: [u64; 3],
    header_offset: u64,
    header_size: u64,
    vmks: Vec<Vmk>,
    fvek: Wrapped,
}

impl Metadata {
    fn read(src: &dyn ReadAt) -> Result<Self> {
        let Some(offsets) = metadata_offsets(src) else { bail!("not a BitLocker drive") };
        let mut bs = [0u8; 512];
        src.read_exact_at(0, &mut bs)?;
        let sector_size = match le16(&bs, 11).unwrap_or(0) {
            0 => 512,
            n => u64::from(n),
        };
        let mut last = None;
        for &off in &offsets {
            match Self::read_copy(src, off, sector_size, offsets) {
                Ok(m) => return Ok(m),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("the BitLocker information could not be read")))
    }

    fn read_copy(src: &dyn ReadAt, off: u64, sector_size: u64, offsets: [u64; 3]) -> Result<Self> {
        let mut block = vec![0u8; METADATA_SIZE as usize];
        crate::source::read_tolerant(src, off, &mut block);
        let b = &block[..];
        if &b[..8] != SIGNATURE || le16(b, 10) != Some(2) {
            bail!("damaged BitLocker information");
        }
        let encrypted_size = le64(b, 16).unwrap_or(0);
        let mut header_offset = le64(b, 56).unwrap_or(0);
        let mut header_size = u64::from(le32(b, 28).unwrap_or(0)) * sector_size;
        let metadata_size = le32(b, 64).unwrap_or(0) as usize;
        if !(48..=METADATA_SIZE as usize - 64).contains(&metadata_size) {
            bail!("damaged BitLocker information");
        }
        let guid: [u8; 16] = b[80..96].try_into()?;
        let method = le16(b, 100).unwrap_or(0);
        let entries = &b[112..64 + metadata_size];
        let mut vmks = Vec::new();
        let mut fvek = None;
        for (ty, value, data) in Entries(entries) {
            match (ty, value) {
                // A volume master key, and how it is protected.
                (0x0002, 0x0008) if data.len() >= 28 => {
                    let mut vmk = Vmk { protection: le16(data, 26).unwrap_or(0xFFFF), ..Vmk::default() };
                    for (_, value, d) in Entries(&data[28..]) {
                        match value {
                            0x0003 if d.len() >= 20 => vmk.salt.copy_from_slice(&d[4..20]),
                            0x0005 => vmk.wrapped = Wrapped::parse(d),
                            0x0001 if d.len() > 4 => vmk.clear_key = Some(d[4..].to_vec()),
                            _ => {}
                        }
                    }
                    vmks.push(vmk);
                }
                (0x0003, 0x0005) if fvek.is_none() => fvek = Wrapped::parse(data),
                // Where the volume's own first sectors went.
                (0x000F, 0x000F) if data.len() >= 16 => {
                    header_offset = le64(data, 0).unwrap_or(header_offset);
                    header_size = le64(data, 8).unwrap_or(header_size);
                }
                _ => {}
            }
        }
        let Some(fvek) = fvek else { bail!("damaged BitLocker information") };
        Ok(Self { guid, sector_size, encrypted_size, method, offsets, header_offset, header_size, vmks, fvek })
    }

    /// The volume master key, from `secret` (a recovery key or password),
    /// or with no secret if BitLocker is suspended.
    fn vmk(&self, secret: Option<&str>) -> Option<Vec<u8>> {
        // Suspended: the key is right there.
        for v in self.vmks.iter().filter(|v| v.protection == PROTECTION_CLEAR_KEY) {
            if let (Some(k), Some(w)) = (&v.clear_key, &v.wrapped)
                && let Some(vmk) = w.unwrap(k)
            {
                return Some(vmk);
            }
        }
        let secret = secret?;
        let recovery = recovery_key(secret);
        for v in &self.vmks {
            let Some(w) = &v.wrapped else { continue };
            let key = match (v.protection, &recovery) {
                (PROTECTION_RECOVERY_PASSWORD, Some(r)) => stretch(&Sha256::digest(r).into(), &v.salt),
                (PROTECTION_PASSWORD, _) => {
                    let utf16: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
                    stretch(&Sha256::digest(Sha256::digest(&utf16)).into(), &v.salt)
                }
                _ => continue,
            };
            if let Some(vmk) = w.unwrap(&key) {
                return Some(vmk);
            }
        }
        None
    }

    /// The keys that decrypt the sectors.
    fn cipher(&self, vmk: &[u8]) -> Result<Cipher> {
        let k = self.fvek.unwrap(vmk).ok_or_else(|| anyhow::anyhow!("damaged BitLocker information"))?;
        let key = |range: std::ops::Range<usize>| Aes::new(k.get(range)?);
        let c = match self.method {
            // AES-CBC with the Elephant diffuser: the sector key follows.
            0x8000 => key(0..16).zip(key(32..48)).map(|(a, b)| Cipher::Elephant(a, b)),
            0x8001 => key(0..32).zip(key(32..64)).map(|(a, b)| Cipher::Elephant(a, b)),
            0x8002 => key(0..16).map(Cipher::Cbc),
            0x8003 => key(0..32).map(Cipher::Cbc),
            0x8004 => key(0..16).zip(key(16..32)).map(|(a, b)| Cipher::Xts(a, b)),
            0x8005 => key(0..32).zip(key(32..64)).map(|(a, b)| Cipher::Xts(a, b)),
            m => bail!("this BitLocker encryption ({m:#06x}) is not supported"),
        };
        c.ok_or_else(|| anyhow::anyhow!("damaged BitLocker information"))
    }
}

/// The entries of a metadata list: (type, value type, data).
struct Entries<'a>(&'a [u8]);

impl<'a> Iterator for Entries<'a> {
    type Item = (u16, u16, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let size = usize::from(le16(self.0, 0)?);
        if size < 8 || size > self.0.len() {
            return None;
        }
        let item = (le16(self.0, 2)?, le16(self.0, 4)?, &self.0[8..size]);
        self.0 = &self.0[size..];
        Some(item)
    }
}

/// The 16 bytes a 48-digit recovery key stands for: eight groups of six
/// digits, each a multiple of 11. Spaces and dashes are optional.
fn recovery_key(s: &str) -> Option<[u8; 16]> {
    let digits: Vec<u32> =
        s.chars().filter(|c| !matches!(c, '-' | ' ')).map(|c| c.to_digit(10)).collect::<Option<_>>()?;
    if digits.len() != 48 {
        return None;
    }
    let mut key = [0u8; 16];
    for (i, group) in digits.chunks(6).enumerate() {
        let n = group.iter().fold(0u32, |n, d| n * 10 + d);
        if n % 11 != 0 || n / 11 > 0xFFFF {
            return None;
        }
        key[i * 2..i * 2 + 2].copy_from_slice(&((n / 11) as u16).to_le_bytes());
    }
    Some(key)
}

/// BitLocker's key stretching: a million rounds of SHA-256.
fn stretch(initial: &[u8; 32], salt: &[u8; 16]) -> Vec<u8> {
    // last hash, initial hash, salt, count
    let mut state = [0u8; 88];
    state[32..64].copy_from_slice(initial);
    state[64..80].copy_from_slice(salt);
    for count in 0..KDF_ITERATIONS {
        state[80..].copy_from_slice(&count.to_le_bytes());
        let h = Sha256::digest(state);
        state[..32].copy_from_slice(&h);
    }
    state[..32].to_vec()
}

/// AES with a 128- or 256-bit key.
#[derive(Clone)]
enum Aes {
    A128(Box<Aes128>),
    A256(Box<Aes256>),
}

impl Aes {
    fn new(key: &[u8]) -> Option<Self> {
        match key.len() {
            16 => Aes128::new_from_slice(key).ok().map(|c| Aes::A128(Box::new(c))),
            32 => Aes256::new_from_slice(key).ok().map(|c| Aes::A256(Box::new(c))),
            _ => None,
        }
    }

    fn encrypt(&self, b: &mut [u8; 16]) {
        let block: &mut aes::Block = (&mut b[..]).try_into().expect("a 16-byte block");
        match self {
            Aes::A128(c) => c.encrypt_block(block),
            Aes::A256(c) => c.encrypt_block(block),
        }
    }

    fn decrypt(&self, b: &mut [u8]) {
        let block: &mut aes::Block = b.try_into().expect("a 16-byte block");
        match self {
            Aes::A128(c) => c.decrypt_block(block),
            Aes::A256(c) => c.decrypt_block(block),
        }
    }
}

/// AES-CCM decryption (12-byte nonce, 16-byte tag, no associated data);
/// `None` if the tag does not match — the key was wrong.
fn ccm_decrypt(aes: &Aes, nonce: &[u8; 12], tag: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    // Counter blocks: flags (L - 1 = 2), nonce, 3-byte counter.
    let counter = |i: u32| {
        let mut a = [0u8; 16];
        a[0] = 2;
        a[1..13].copy_from_slice(nonce);
        a[13..].copy_from_slice(&i.to_be_bytes()[1..]);
        aes.encrypt(&mut a);
        a
    };
    let mut plain = data.to_vec();
    for (i, chunk) in plain.chunks_mut(16).enumerate() {
        let s = counter(i as u32 + 1);
        chunk.iter_mut().zip(s).for_each(|(p, s)| *p ^= s);
    }
    // CBC-MAC over B0 (flags: 16-byte tag, L = 3) and the plaintext.
    let mut mac = [0u8; 16];
    mac[0] = 0x3A;
    mac[1..13].copy_from_slice(nonce);
    mac[13..].copy_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
    aes.encrypt(&mut mac);
    for chunk in plain.chunks(16) {
        chunk.iter().zip(&mut mac).for_each(|(p, m)| *m ^= p);
        aes.encrypt(&mut mac);
    }
    let s0 = counter(0);
    let ok = mac.iter().zip(s0).zip(tag).fold(0u8, |acc, ((m, s), t)| acc | (m ^ s ^ t)) == 0;
    ok.then_some(plain)
}

/// How sectors are encrypted.
#[derive(Clone)]
enum Cipher {
    /// Data key, tweak key.
    Xts(Aes, Aes),
    Cbc(Aes),
    /// Data key, sector key.
    Elephant(Aes, Aes),
}

impl Cipher {
    /// Decrypts one sector, which starts at byte `offset` of the volume.
    fn decrypt(&self, sector: &mut [u8], offset: u64, sector_size: u64) {
        match self {
            Cipher::Xts(data, tweak) => {
                let mut t = [0u8; 16];
                t[..8].copy_from_slice(&(offset / sector_size).to_le_bytes());
                tweak.encrypt(&mut t);
                for block in sector.as_chunks_mut::<16>().0 {
                    block.iter_mut().zip(&t).for_each(|(b, t)| *b ^= t);
                    data.decrypt(block);
                    block.iter_mut().zip(&t).for_each(|(b, t)| *b ^= t);
                    // t *= x in GF(2^128)
                    let carry = t[15] >> 7;
                    for i in (1..16).rev() {
                        t[i] = (t[i] << 1) | (t[i - 1] >> 7);
                    }
                    t[0] = (t[0] << 1) ^ (carry * 0x87);
                }
            }
            Cipher::Cbc(data) => cbc_decrypt(data, sector, offset),
            Cipher::Elephant(data, sector_key) => {
                cbc_decrypt(data, sector, offset);
                let words = sector.len() / 4;
                let mut d: Vec<u32> = sector.as_chunks::<4>().0.iter().map(|w| u32::from_le_bytes(*w)).collect();
                diffuser_b_decrypt(&mut d[..words]);
                diffuser_a_decrypt(&mut d[..words]);
                for (w, out) in d.iter().zip(sector.as_chunks_mut::<4>().0) {
                    out.copy_from_slice(&w.to_le_bytes());
                }
                // XOR with the sector key: E(offset), E(offset with 0x80 at byte 15).
                let mut ks = [0u8; 32];
                let mut e = [0u8; 16];
                e[..8].copy_from_slice(&offset.to_le_bytes());
                let mut k1 = e;
                sector_key.encrypt(&mut k1);
                e[15] = 0x80;
                sector_key.encrypt(&mut e);
                ks[..16].copy_from_slice(&k1);
                ks[16..].copy_from_slice(&e);
                for chunk in sector.as_chunks_mut::<32>().0 {
                    chunk.iter_mut().zip(ks).for_each(|(b, k)| *b ^= k);
                }
            }
        }
    }
}

/// AES-CBC with the IV that BitLocker uses: the sector's byte offset,
/// encrypted with the data key.
fn cbc_decrypt(aes: &Aes, sector: &mut [u8], offset: u64) {
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(&offset.to_le_bytes());
    aes.encrypt(&mut iv);
    for block in sector.as_chunks_mut::<16>().0 {
        let c = *block;
        aes.decrypt(block);
        block.iter_mut().zip(iv).for_each(|(b, v)| *b ^= v);
        iv = c;
    }
}

// The Elephant diffuser (Niels Ferguson, "AES-CBC + Elephant diffuser",
// Microsoft 2006), decryption direction, as in Linux dm-crypt.
fn diffuser_a_decrypt(d: &mut [u32]) {
    let n = d.len();
    for _ in 0..5 {
        let (mut i1, mut i2, mut i3) = (0, n - 2, n - 5);
        while i1 < n - 1 {
            d[i1] = d[i1].wrapping_add(d[i2] ^ d[i3].rotate_left(9));
            i1 += 1;
            i2 += 1;
            i3 += 1;
            if i3 >= n {
                i3 -= n;
            }
            d[i1] = d[i1].wrapping_add(d[i2] ^ d[i3]);
            i1 += 1;
            i2 += 1;
            i3 += 1;
            if i2 >= n {
                i2 -= n;
            }
            d[i1] = d[i1].wrapping_add(d[i2] ^ d[i3].rotate_left(13));
            i1 += 1;
            i2 += 1;
            i3 += 1;
            d[i1] = d[i1].wrapping_add(d[i2] ^ d[i3]);
            i1 += 1;
            i2 += 1;
            i3 += 1;
        }
    }
}

fn diffuser_b_decrypt(d: &mut [u32]) {
    let n = d.len();
    for _ in 0..3 {
        let (mut i1, mut i2, mut i3) = (0, 2, 5);
        while i1 < n - 1 {
            d[i1] = d[i1].wrapping_add(d[i2] ^ d[i3]);
            i1 += 1;
            i2 += 1;
            i3 += 1;
            d[i1] = d[i1].wrapping_add(d[i2] ^ d[i3].rotate_left(10));
            i1 += 1;
            i2 += 1;
            i3 += 1;
            if i2 >= n {
                i2 -= n;
            }
            d[i1] = d[i1].wrapping_add(d[i2] ^ d[i3]);
            i1 += 1;
            i2 += 1;
            i3 += 1;
            if i3 >= n {
                i3 -= n;
            }
            d[i1] = d[i1].wrapping_add(d[i2] ^ d[i3].rotate_left(25));
            i1 += 1;
            i2 += 1;
            i3 += 1;
        }
    }
}

/// A BitLocker volume read decrypted.
pub struct Unlocked {
    raw: Source,
    meta: Metadata,
    cipher: Cipher,
    /// Read never-used (blank) sectors as blank, instead of decrypting them
    /// into noise.
    keep_blank: bool,
}

impl Unlocked {
    /// Where the plaintext sector at `pos` comes from.
    fn plan(&self, pos: u64) -> Plan {
        let m = &self.meta;
        if pos < m.header_size {
            return Plan::Decrypt(m.header_offset + pos);
        }
        let hidden = m.offsets.iter().any(|&o| (o..o + METADATA_SIZE).contains(&pos))
            || (m.header_offset..m.header_offset + m.header_size).contains(&pos);
        if hidden {
            Plan::Zero
        } else if m.encrypted_size > 0 && pos >= m.encrypted_size {
            // Encryption was still in progress: the rest is plain.
            Plan::Plain
        } else {
            Plan::Decrypt(pos)
        }
    }
}

enum Plan {
    Zero,
    Plain,
    Decrypt(u64),
}

impl ReadAt for Unlocked {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.size();
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let ss = self.meta.sector_size;
        let want = (buf.len() as u64).min(size - offset) as usize;
        let start = offset - offset % ss;
        let end = (offset + want as u64).div_ceil(ss) * ss;
        let mut sectors = vec![0u8; (end - start) as usize];
        // One read for the whole span; the moved header is read separately.
        let n = self.raw.read_at(start, &mut sectors)?;
        let mut done = 0usize;
        for (i, sector) in sectors.chunks_exact_mut(ss as usize).enumerate() {
            let pos = start + i as u64 * ss;
            match self.plan(pos) {
                Plan::Zero => sector.fill(0),
                Plan::Plain => {}
                Plan::Decrypt(from) => {
                    if from != pos {
                        self.raw.read_exact_at(from, sector)?;
                    }
                    if !(self.keep_blank && sector.iter().all(|&b| b == 0)) {
                        self.cipher.decrypt(sector, from, ss);
                    }
                }
            }
            done = (i + 1) * ss as usize;
            if done >= n {
                break;
            }
        }
        let got = done.min(n).saturating_sub((offset - start) as usize).min(want);
        if got == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let from = (offset - start) as usize;
        buf[..got].copy_from_slice(&sectors[from..from + got]);
        Ok(got)
    }

    fn size(&self) -> u64 {
        self.raw.size()
    }
}

/// Opens `raw` with `secret` (a recovery key or password; `None` works
/// only while BitLocker is suspended).
pub fn unlock(raw: Source, secret: Option<&str>) -> Result<Unlocked> {
    let meta = Metadata::read(raw.as_ref())?;
    let Some(vmk) = meta.vmk(secret) else {
        bail!(match secret {
            Some(_) => "this recovery key or password does not open the drive",
            None => "this drive is locked with BitLocker",
        })
    };
    let cipher = meta.cipher(&vmk)?;
    Ok(Unlocked { raw, meta, cipher, keep_blank: true })
}

/// Keys the user gave, and the volumes they opened (by volume GUID).
struct Keyring {
    secrets: Vec<String>,
    opened: HashMap<[u8; 16], (Metadata, Cipher)>,
}

static KEYRING: Mutex<Option<Keyring>> = Mutex::new(None);

/// Remembers a recovery key or password for [`open`], if it opens `raw`.
pub fn add_key(raw: Source, secret: &str) -> Result<()> {
    let u = unlock(raw, Some(secret))?;
    let mut ring = KEYRING.lock().unwrap_or_else(|e| e.into_inner());
    let ring = ring.get_or_insert_with(|| Keyring { secrets: Vec::new(), opened: HashMap::new() });
    if !ring.secrets.iter().any(|s| s == secret) {
        ring.secrets.push(secret.to_string());
    }
    ring.opened.insert(u.meta.guid, (u.meta, u.cipher));
    Ok(())
}

/// Remembers a recovery key or password without checking it (for drives
/// not opened yet: `--bitlocker-key` on the command line).
pub fn remember_key(secret: &str) {
    let mut ring = KEYRING.lock().unwrap_or_else(|e| e.into_inner());
    let ring = ring.get_or_insert_with(|| Keyring { secrets: Vec::new(), opened: HashMap::new() });
    if !ring.secrets.iter().any(|s| s == secret) {
        ring.secrets.push(secret.to_string());
    }
}

/// `raw` decrypted, if it is a BitLocker volume that one of the keys given
/// so far opens (or that is suspended). `None` otherwise.
pub fn open(raw: &Source) -> Option<Source> {
    if !is_bitlocker(raw.as_ref()) {
        return None;
    }
    let meta = Metadata::read(raw.as_ref()).ok()?;
    let mut ring = KEYRING.lock().unwrap_or_else(|e| e.into_inner());
    let ring = ring.get_or_insert_with(|| Keyring { secrets: Vec::new(), opened: HashMap::new() });
    if !ring.opened.contains_key(&meta.guid) {
        let vmk =
            std::iter::once(None).chain(ring.secrets.iter().map(|s| Some(s.as_str()))).find_map(|s| meta.vmk(s))?;
        let cipher = meta.cipher(&vmk).ok()?;
        ring.opened.insert(meta.guid, (meta.clone(), cipher));
    }
    let (meta, cipher) = ring.opened.get(&meta.guid)?.clone();
    Some(Arc::new(Unlocked { raw: raw.clone(), meta, cipher, keep_blank: true }))
}

/// `disk` with its unlocked BitLocker partitions read decrypted, so that
/// everything — the deep search too — sees their contents.
pub fn overlay(disk: Source, partitions: &[crate::partition::Partition]) -> Source {
    let parts: Vec<(u64, u64, Source)> = partitions
        .iter()
        .filter(|p| p.device.is_none() && p.len > 0)
        .filter_map(|p| {
            let raw: Source = Arc::new(crate::source::SubSource::new(disk.clone(), p.start, p.len));
            open(&raw).map(|d| (p.start, p.len, d))
        })
        .collect();
    if parts.is_empty() { disk } else { Arc::new(Overlay { disk, parts }) }
}

struct Overlay {
    disk: Source,
    /// Start, length, decrypted contents.
    parts: Vec<(u64, u64, Source)>,
}

impl ReadAt for Overlay {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if let Some((start, len, src)) = self.parts.iter().find(|(s, l, _)| (*s..s + l).contains(&offset)) {
            let n = (buf.len() as u64).min(start + len - offset) as usize;
            return src.read_at(offset - start, &mut buf[..n]);
        }
        // Up to the next decrypted partition.
        let next = self.parts.iter().map(|(s, _, _)| *s).filter(|&s| s > offset).min().unwrap_or(u64::MAX);
        let n = (buf.len() as u64).min(next - offset) as usize;
        self.disk.read_at(offset, &mut buf[..n])
    }

    fn size(&self) -> u64 {
        self.disk.size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_keys() {
        let k = recovery_key("235818-357951-253979-013365-241120-245575-342914-591910").unwrap();
        // 235818 / 11 = 0x53BE, 357951 / 11 = 0x7F1D
        assert_eq!(&k[..4], &[0xBE, 0x53, 0x1D, 0x7F]);
        assert_eq!(recovery_key("235818 357951 253979 013365 241120 245575 342914 591910"), Some(k));
        assert_eq!(recovery_key("235818357951253979013365241120245575342914591910"), Some(k));
        // Not a multiple of 11, too short, not digits.
        assert!(recovery_key("235819-357951-253979-013365-241120-245575-342914-591910").is_none());
        assert!(recovery_key("235818-357951").is_none());
        assert!(recovery_key("anaconda").is_none());
    }

    /// A test volume from cryptsetup's test suite (`tests/data/*.sectors`).
    fn volume(name: &str) -> Source {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name);
        let raw = std::fs::read(&path).unwrap();
        let size = u64::from_le_bytes(raw[8..16].try_into().unwrap()) as usize;
        let mut img = vec![0u8; size];
        for s in raw[16..].as_chunks::<{ 8 + 512 }>().0 {
            let at = u64::from_le_bytes(s[..8].try_into().unwrap()) as usize;
            img[at..at + 512].copy_from_slice(&s[8..]);
        }
        Arc::new(crate::source::MemSource(img))
    }

    /// The SHA-256 of the whole volume decrypted, as cryptsetup reads it
    /// (blank sectors decrypted into noise like everything else).
    fn decrypted_sha256(raw: Source, secret: Option<&str>) -> String {
        let mut u = unlock(raw, secret).unwrap();
        u.keep_blank = false;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut pos = 0;
        while pos < u.size() {
            let n = (u.size() - pos).min(buf.len() as u64) as usize;
            u.read_exact_at(pos, &mut buf[..n]).unwrap();
            h.update(&buf[..n]);
            pos += n as u64;
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn bitlocker_volumes_decrypt_exactly_like_cryptsetup() {
        let cases = [
            // XTS (Windows 10 and later), opened with the recovery key.
            (
                "bitlocker-xts-128.sectors",
                "235818-357951-253979-013365-241120-245575-342914-591910",
                "674e3a976927fd62f3fc26df2c695cac75b8d364e3b45393717efa971f16db0f",
            ),
            // ... and with the password.
            (
                "bitlocker-xts-256.sectors",
                "anaconda",
                "5bb6ff5acbded10be990c6fa208ab479934a08bc2e88740a1aa2642af2f42025",
            ),
            (
                "bitlocker-cbc-128.sectors",
                "anaconda",
                "04500a8120ba355ed206284e03e26e59b7e1f1832868e1d69bb47023ebd3460f",
            ),
            // Windows 7.
            (
                "bitlocker-elephant-128.sectors",
                "529573-278784-259347-197835-171457-264044-610280-313269",
                "b18e4f956295bc0f327e551322261fb9c74ac0d3ce58bf3b806e98474e1619ea",
            ),
            (
                "bitlocker-elephant-256.sectors",
                "anaconda",
                "0af06f010fe21522bdd77f8d2d3cb0ad5fceaf2729295ff0fd50e65adfa0b7b3",
            ),
            // A USB stick (BitLocker To Go), and 4K sectors.
            (
                "bitlocker-togo-xts-128.sectors",
                "anaconda",
                "5954795eb41764b59a10d86c26fd3b43fb6d89f433c8edc1e8fd48067d198591",
            ),
            (
                "bitlocker-xts-128-4k.sectors",
                "486552-140030-675719-163900-264671-413787-580239-152614",
                "b4c0416ae643537207413ed78d4bcadae697bb86a6262864ac00afda01312277",
            ),
        ];
        std::thread::scope(|s| {
            for (name, secret, sha) in cases {
                s.spawn(move || {
                    let raw = volume(name);
                    assert!(is_bitlocker(raw.as_ref()), "{name}");
                    assert_eq!(decrypted_sha256(raw, Some(secret)), sha, "{name}");
                });
            }
        });
    }

    #[test]
    fn suspended_bitlocker_needs_no_key_and_wrong_keys_are_refused() {
        let raw = volume("bitlocker-suspended.sectors");
        assert_eq!(decrypted_sha256(raw, None), "f574a5254d31e9f27dc4ee440290875886c6c569cf02dc100e91a5c0cddaa4e1");
        let raw = volume("bitlocker-xts-128.sectors");
        assert!(unlock(raw.clone(), None).is_err());
        assert!(unlock(raw.clone(), Some("wrong password")).is_err());
        // A valid-looking recovery key of another drive.
        assert!(unlock(raw, Some("404558-436711-420860-678557-638220-018909-039941-695321")).is_err());
    }
}
