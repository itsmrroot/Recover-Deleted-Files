//! LVM2 (Linux Logical Volume Manager), as used by Synology and QNAP NAS
//! drives and many Linux installs. A physical volume (PV) has a label near
//! its start pointing at a metadata area; the newest metadata, plain text,
//! describes the volume group: its PVs, extent size and logical volumes
//! (LVs), each made of segments of extents on the PVs (linear or striped).

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use crate::bytes::{le32, le64};
use crate::source::{ReadAt, Source};

const MDA_MAGIC: &[u8; 16] = b" LVM2 x[5A%r0N*>";

/// A physical volume: its UUID (without dashes) and the newest metadata
/// text found in its metadata area.
#[derive(Debug, Clone)]
pub struct Pv {
    pub uuid: String,
    pub metadata: String,
}

/// Reads the PV label of `src`, if it is an LVM physical volume.
pub fn pv(src: &dyn ReadAt) -> Option<Pv> {
    let head = src.read_vec(0, 4 * 512).ok()?;
    let label = (0..4).map(|i| i * 512).find(|&at| head[at..].starts_with(b"LABELONE"))?;
    if &head[label + 24..label + 32] != b"LVM2 001" {
        return None;
    }
    let pvh = label + le32(&head, label + 20)? as usize;
    let uuid = String::from_utf8_lossy(head.get(pvh..pvh + 32)?).to_string();
    // Data areas, then metadata areas: (offset, size) pairs ending with zeros.
    let mut at = pvh + 40;
    let mut lists = [Vec::new(), Vec::new()];
    for list in &mut lists {
        loop {
            let (off, size) = (le64(&head, at)?, le64(&head, at + 8)?);
            at += 16;
            if off == 0 && size == 0 {
                break;
            }
            list.push((off, size));
        }
    }
    let metadata = lists[1].iter().find_map(|&(off, size)| read_mda(src, off, size)).unwrap_or_default();
    Some(Pv { uuid, metadata })
}

/// The current metadata text of a metadata area (a circular buffer after
/// a 512-byte header).
fn read_mda(src: &dyn ReadAt, at: u64, size: u64) -> Option<String> {
    let h = src.read_vec(at, 512).ok()?;
    if &h[4..20] != MDA_MAGIC {
        return None;
    }
    let off = le64(&h, 40)?;
    let len = le64(&h, 48)?;
    if off == 0 || len == 0 || len > size {
        return None;
    }
    let mut text = vec![0u8; len as usize];
    let first = (size - off).min(len);
    src.read_exact_at(at + off, &mut text[..first as usize]).ok()?;
    if first < len {
        // Wrapped around to just after the header.
        src.read_exact_at(at + 512, &mut text[first as usize..]).ok()?;
    }
    Some(String::from_utf8_lossy(&text).trim_end_matches('\0').to_string())
}

// ----------------------------------------------------------------------
// The metadata text: `name { key = value ... }`, values being numbers,
// "strings" or [lists].

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Num(i64),
    Str(String),
    List(Vec<Value>),
    Section(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Section(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn num(&self, key: &str) -> Option<u64> {
        match self.get(key)? {
            Value::Num(n) => u64::try_from(*n).ok(),
            _ => None,
        }
    }

    fn text(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    fn sections(&self) -> impl Iterator<Item = (&str, &Value)> {
        let items = match self {
            Value::Section(items) => items.as_slice(),
            _ => &[],
        };
        items.iter().filter(|(_, v)| matches!(v, Value::Section(_))).map(|(k, v)| (k.as_str(), v))
    }
}

pub fn parse(text: &str) -> Value {
    let tokens = tokenize(text);
    let mut pos = 0;
    Value::Section(section(&tokens, &mut pos))
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Str(String),
    Punct(char),
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '#' => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                s.push(n);
                            }
                        }
                        c => s.push(c),
                    }
                }
                out.push(Token::Str(s));
            }
            '{' | '}' | '[' | ']' | '=' | ',' => {
                out.push(Token::Punct(c));
                chars.next();
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            _ => {
                let mut w = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || "{}[]=,#\"".contains(c) {
                        break;
                    }
                    w.push(c);
                    chars.next();
                }
                out.push(Token::Word(w));
            }
        }
    }
    out
}

fn section(t: &[Token], pos: &mut usize) -> Vec<(String, Value)> {
    let mut items = Vec::new();
    while *pos < t.len() {
        match &t[*pos] {
            Token::Punct('}') => {
                *pos += 1;
                break;
            }
            Token::Word(name) => {
                let name = name.clone();
                *pos += 1;
                match t.get(*pos) {
                    Some(Token::Punct('{')) => {
                        *pos += 1;
                        items.push((name, Value::Section(section(t, pos))));
                    }
                    Some(Token::Punct('=')) => {
                        *pos += 1;
                        items.push((name, value(t, pos)));
                    }
                    _ => {}
                }
            }
            _ => *pos += 1,
        }
    }
    items
}

fn value(t: &[Token], pos: &mut usize) -> Value {
    let v = match t.get(*pos) {
        Some(Token::Str(s)) => Value::Str(s.clone()),
        Some(Token::Word(w)) => w.parse().map(Value::Num).unwrap_or_else(|_| Value::Str(w.clone())),
        Some(Token::Punct('[')) => {
            *pos += 1;
            let mut list = Vec::new();
            while let Some(tok) = t.get(*pos) {
                match tok {
                    Token::Punct(']') => break,
                    Token::Punct(',') => *pos += 1,
                    _ => {
                        list.push(value(t, pos));
                    }
                }
            }
            Value::List(list)
        }
        _ => Value::Str(String::new()),
    };
    *pos += 1;
    v
}

// ----------------------------------------------------------------------
// Logical volumes

/// A logical volume, readable.
pub struct Lv {
    pub name: String,
    pub vg: String,
    pub source: Source,
}

#[derive(Clone)]
struct Segment {
    /// First byte of the segment in the LV, and its length.
    start: u64,
    len: u64,
    /// Bytes per stripe chunk (0: linear).
    stripe: u64,
    /// For each stripe: the PV and the byte offset of its area on it.
    areas: Vec<(Source, u64)>,
}

struct LvSource {
    segments: Vec<Segment>,
    size: u64,
}

impl ReadAt for LvSource {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let Some(s) = self.segments.iter().find(|s| (s.start..s.start + s.len).contains(&offset)) else {
            // A hole (an unsupported segment): zeros.
            let next = self.segments.iter().map(|s| s.start).filter(|&s| s > offset).min().unwrap_or(self.size);
            let n = (buf.len() as u64).min(next - offset) as usize;
            buf[..n].fill(0);
            return Ok(n);
        };
        let within = offset - s.start;
        let max = (buf.len() as u64).min(s.start + s.len - offset);
        if s.areas.len() == 1 || s.stripe == 0 {
            let (pv, at) = &s.areas[0];
            let n = max as usize;
            pv.read_exact_at(at + within, &mut buf[..n])?;
            return Ok(n);
        }
        let k = within / s.stripe;
        let in_chunk = within % s.stripe;
        let n = max.min(s.stripe - in_chunk) as usize;
        let count = s.areas.len() as u64;
        let (pv, at) = &s.areas[(k % count) as usize];
        pv.read_exact_at(at + (k / count) * s.stripe + in_chunk, &mut buf[..n])?;
        Ok(n)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// The logical volumes of every volume group whose physical volumes are
/// among `pvs` (each a source and its label). Volumes of unsupported kinds
/// (thin, snapshots, LVM's own RAID) are left out.
pub fn volumes(pvs: &[(Source, Pv)]) -> Vec<Lv> {
    let by_uuid: HashMap<&str, &Source> = pvs.iter().map(|(s, p)| (p.uuid.as_str(), s)).collect();
    let mut seen_vgs = Vec::new();
    let mut out = Vec::new();
    // The PV with the newest metadata speaks for its group.
    let mut sorted: Vec<&(Source, Pv)> = pvs.iter().collect();
    sorted.sort_by_key(|(_, p)| std::cmp::Reverse(seqno(&p.metadata)));
    for (_, pv) in sorted {
        let root = parse(&pv.metadata);
        let Some((vg_name, vg)) = root.sections().find(|(_, v)| v.get("physical_volumes").is_some()) else {
            continue;
        };
        if seen_vgs.contains(&vg_name.to_string()) {
            continue;
        }
        seen_vgs.push(vg_name.to_string());
        let extent = vg.num("extent_size").unwrap_or(8192) * 512;
        let mut pv_map: HashMap<&str, (Source, u64)> = HashMap::new();
        if let Some(pvs) = vg.get("physical_volumes") {
            for (name, p) in pvs.sections() {
                let Some(id) = p.text("id") else { continue };
                if let Some(src) = by_uuid.get(id.replace('-', "").as_str()) {
                    pv_map.insert(name, ((*src).clone(), p.num("pe_start").unwrap_or(2048) * 512));
                }
            }
        }
        let Some(lvs) = vg.get("logical_volumes") else { continue };
        for (name, lv) in lvs.sections() {
            let mut segments = Vec::new();
            let mut size = 0u64;
            for (key, seg) in lv.sections() {
                if !key.starts_with("segment") {
                    continue;
                }
                let start = seg.num("start_extent").unwrap_or(0) * extent;
                let len = seg.num("extent_count").unwrap_or(0) * extent;
                size = size.max(start + len);
                if seg.text("type") != Some("striped") {
                    log::info!("LVM volume {vg_name}/{name}: segment type {:?} not supported", seg.text("type"));
                    continue;
                }
                let Some(Value::List(stripes)) = seg.get("stripes") else { continue };
                let mut areas = Vec::new();
                for pair in stripes.chunks(2) {
                    let (Some(Value::Str(pv)), Some(Value::Num(pe))) = (pair.first(), pair.get(1)) else { continue };
                    if let Some((src, pe_start)) = pv_map.get(pv.as_str()) {
                        areas.push((src.clone(), pe_start + *pe as u64 * extent));
                    }
                }
                if areas.len() * 2 != stripes.len() {
                    // A physical volume is missing.
                    continue;
                }
                let stripe = seg.num("stripe_size").unwrap_or(0) * 512;
                segments.push(Segment { start, len, stripe, areas });
            }
            if segments.is_empty() || size == 0 {
                continue;
            }
            out.push(Lv {
                name: name.to_string(),
                vg: vg_name.to_string(),
                source: Arc::new(LvSource { segments, size }),
            });
        }
    }
    out
}

fn seqno(metadata: &str) -> u64 {
    parse(metadata).sections().find_map(|(_, v)| v.num("seqno")).unwrap_or(0)
}

/// Whether an LV is one contiguous piece of a single PV: then its place on
/// that PV (offset, length).
pub fn contiguous(pv: &Pv, lv_name: &str) -> Option<(u64, u64)> {
    let root = parse(&pv.metadata);
    let (_, vg) = root.sections().find(|(_, v)| v.get("physical_volumes").is_some())?;
    let extent = vg.num("extent_size")? * 512;
    let pvs = vg.get("physical_volumes")?;
    let (pv_name, me) = pvs.sections().find(|(_, p)| p.text("id").is_some_and(|i| i.replace('-', "") == pv.uuid))?;
    let lv = vg.get("logical_volumes")?.get(lv_name)?;
    let segs: Vec<&Value> = lv.sections().filter(|(k, _)| k.starts_with("segment")).map(|(_, v)| v).collect();
    let [seg] = segs.as_slice() else { return None };
    if seg.text("type") != Some("striped") || seg.num("stripe_count") != Some(1) {
        return None;
    }
    let Some(Value::List(stripes)) = seg.get("stripes") else { return None };
    let (Some(Value::Str(name)), Some(Value::Num(pe))) = (stripes.first(), stripes.get(1)) else { return None };
    if name != pv_name {
        return None;
    }
    let start = me.num("pe_start")? * 512 + *pe as u64 * extent;
    Some((start, seg.num("extent_count")? * extent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_text() {
        let v = parse(
            r#"vg1000 {
                id = "abc"   # a comment
                seqno = 5
                extent_size = 2048
                status = ["RESIZEABLE", "READ", "WRITE"]
                physical_volumes { pv0 { id = "x-y" pe_start = 2048 } }
            }"#,
        );
        let vg = v.get("vg1000").unwrap();
        assert_eq!(vg.num("seqno"), Some(5));
        assert_eq!(vg.text("id"), Some("abc"));
        assert_eq!(
            vg.get("status"),
            Some(&Value::List(vec![
                Value::Str("RESIZEABLE".into()),
                Value::Str("READ".into()),
                Value::Str("WRITE".into())
            ]))
        );
        assert_eq!(vg.get("physical_volumes").unwrap().get("pv0").unwrap().text("id"), Some("x-y"));
    }
}
