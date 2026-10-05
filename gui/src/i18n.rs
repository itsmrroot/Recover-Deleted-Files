//! Interface languages (English, German, Arabic) and right-to-left text.
//!
//! Strings are written in English in the code and looked up in the tables in
//! `translations.rs`:
//! - [`tr`] for a fixed label, ready to display;
//! - [`trf`] for a label with `{name}` placeholders, ready to display;
//! - [`trn`] for a label that depends on a count;
//! - [`trl`] for text that is wrapped by [`crate::theme::paragraph`].
//!
//! egui shapes Arabic (joined letters) but has no bidirectional layout: it
//! cuts a line into runs by font, places the runs left to right, and the
//! shaper reverses every run that contains Arabic. [`visual`] reorders a line
//! with the Unicode bidi algorithm and then pre-reverses the Arabic runs, so
//! that egui ends up drawing the correct visual order — including numbers
//! and Latin file names inside Arabic sentences. This relies on the Arabic
//! font coming first in the font list while Arabic is selected (see
//! `theme::install_fonts`).

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};
use skrifa::MetadataProvider;
use unicode_bidi::{BidiInfo, Level};

use crate::translations;

pub const ARABIC_FONT: &[u8] = include_bytes!("../assets/fonts/NotoSansArabic-Regular.ttf");
pub const ARABIC_FONT_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/NotoSansArabic-SemiBold.ttf");

/// The language chosen in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Language {
    /// The operating system's language, or English if it is not supported.
    #[default]
    System,
    English,
    German,
    Arabic,
}

impl Language {
    pub const ALL: [Language; 4] = [Language::System, Language::English, Language::German, Language::Arabic];

    /// The name shown in the language picker: each language in its own words.
    pub fn label(self) -> Cow<'static, str> {
        match self {
            Language::System => Cow::Borrowed(tr("System language")),
            Language::English => Cow::Borrowed("English"),
            Language::German => Cow::Borrowed("Deutsch"),
            Language::Arabic => visual("العربية"),
        }
    }

    fn resolve(self) -> Lang {
        match self {
            Language::English => Lang::En,
            Language::German => Lang::De,
            Language::Arabic => Lang::Ar,
            Language::System => {
                let locale = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase();
                match locale.get(..2) {
                    Some("de") => Lang::De,
                    Some("ar") => Lang::Ar,
                    _ => Lang::En,
                }
            }
        }
    }
}

/// A language with translations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Lang {
    En = 0,
    De = 1,
    Ar = 2,
}

static CURRENT: AtomicU8 = AtomicU8::new(Lang::En as u8);

/// Switches the interface language. Returns the language now in use.
pub fn set_language(l: Language) -> Lang {
    let lang = l.resolve();
    CURRENT.store(lang as u8, Ordering::Relaxed);
    lang
}

pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::De,
        2 => Lang::Ar,
        _ => Lang::En,
    }
}

pub fn is_rtl() -> bool {
    current() == Lang::Ar
}

/// The translation of `en`, in logical (typing) order.
fn lookup(en: &'static str) -> &'static str {
    static TABLES: LazyLock<[HashMap<&'static str, &'static str>; 2]> = LazyLock::new(|| {
        [translations::GERMAN.iter().copied().collect(), translations::ARABIC.iter().copied().collect()]
    });
    match current() {
        Lang::En => en,
        Lang::De => TABLES[0].get(en).copied().unwrap_or(en),
        Lang::Ar => TABLES[1].get(en).copied().unwrap_or(en),
    }
}

/// A fixed label, translated and ready to display.
pub fn tr(en: &'static str) -> &'static str {
    let s = lookup(en);
    if !needs_reordering(s) {
        return s;
    }
    // The set of labels is small and fixed, so their display forms are kept.
    static CACHE: LazyLock<Mutex<HashMap<&'static str, &'static str>>> = LazyLock::new(Default::default);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.entry(s).or_insert_with(|| Box::leak(visual(s).into_owned().into_boxed_str()))
}

/// Text that will be wrapped by `theme::paragraph`: translated, but left in
/// logical order because lines must be broken before they are reordered.
pub fn trl(en: &'static str) -> &'static str {
    lookup(en)
}

/// Like [`trl`], with `{name}` placeholders filled in.
pub fn trlf(en: &'static str, args: &[(&str, &dyn Display)]) -> String {
    let mut s = lookup(en).to_string();
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), &value.to_string());
    }
    s
}

/// A label with `{name}` placeholders, translated and ready to display.
pub fn trf(en: &'static str, args: &[(&str, &dyn Display)]) -> String {
    let s = trlf(en, args);
    match visual(&s) {
        Cow::Borrowed(_) => s,
        Cow::Owned(v) => v,
    }
}

/// A label about `n` things: `one` when there is exactly one, else `many`.
/// Both may use `{n}`.
pub fn trn(n: impl Into<u64>, one: &'static str, many: &'static str) -> String {
    let n = n.into();
    trf(if n == 1 { one } else { many }, &[("n", &n)])
}

/// An icon followed by a translated label.
pub fn icon_label(icon: &str, en: &'static str) -> String {
    let s = format!("{icon} {}", lookup(en));
    match visual(&s) {
        Cow::Borrowed(_) => s,
        Cow::Owned(v) => v,
    }
}

// ---------------------------------------------------------------------------
// Right-to-left text

fn is_arabic(c: char) -> bool {
    matches!(c, '\u{0600}'..='\u{06FF}' | '\u{0750}'..='\u{077F}' | '\u{08A0}'..='\u{08FF}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}')
}

fn needs_reordering(s: &str) -> bool {
    s.chars().any(is_arabic)
}

/// Whether egui draws `c` with the Arabic font (it comes first while the
/// interface is Arabic, so: whether that font has the character).
fn in_arabic_font(c: char) -> bool {
    static FONT: LazyLock<Option<skrifa::FontRef<'static>>> = LazyLock::new(|| skrifa::FontRef::new(ARABIC_FONT).ok());
    FONT.as_ref().is_some_and(|f| f.charmap().map(c).is_some())
}

/// Converts text to the order in which egui must receive it to draw it
/// correctly as right-to-left text. Text without Arabic is returned as is.
pub fn visual(s: &str) -> Cow<'_, str> {
    if !needs_reordering(s) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        visual_line(line, &mut out);
    }
    Cow::Owned(out)
}

fn visual_line(line: &str, out: &mut String) {
    let info = BidiInfo::new(line, Some(Level::rtl()));
    let Some(para) = info.paragraphs.first() else {
        out.push_str(line);
        return;
    };
    // The order in which the characters must appear on screen, left to right,
    // and whether each one is part of right-to-left text.
    let (_, runs) = info.visual_runs(para, para.range.clone());
    let mut screen: Vec<(char, bool)> = Vec::with_capacity(line.len());
    for run in runs {
        let rtl = info.levels[run.start].is_rtl();
        let chars = line[run.clone()].chars().map(|c| (c, rtl));
        if rtl { screen.extend(chars.rev()) } else { screen.extend(chars) }
    }
    // egui shapes each stretch of characters that share a font separately and
    // the shaper reverses (and mirrors) those containing Arabic: reverse them
    // beforehand. Brackets in right-to-left text that the shaper does not see
    // as Arabic are mirrored here instead.
    let mut start = 0;
    while start < screen.len() {
        let arabic_font = in_arabic_font(screen[start].0);
        let end = screen[start..]
            .iter()
            .position(|&(c, _)| in_arabic_font(c) != arabic_font)
            .map_or(screen.len(), |n| start + n);
        let piece = &screen[start..end];
        if arabic_font && piece.iter().any(|&(c, _)| is_arabic(c)) {
            out.extend(piece.iter().rev().map(|&(c, _)| c));
        } else {
            out.extend(piece.iter().map(|&(c, rtl)| if rtl { mirror(c) } else { c }));
        }
        start = end;
    }
}

/// The mirror image of a bracket, as drawn in right-to-left text.
fn mirror(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What egui does with a line: cut it into runs by font and reverse (and
    /// mirror) the runs that contain Arabic.
    fn egui_draws(s: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::new();
        let mut start = 0;
        while start < chars.len() {
            let f = in_arabic_font(chars[start]);
            let end = chars[start..].iter().position(|&c| in_arabic_font(c) != f).map_or(chars.len(), |n| start + n);
            let run = &chars[start..end];
            if run.iter().any(|&c| is_arabic(c)) {
                out.extend(run.iter().rev().map(|&c| mirror(c)));
            } else {
                out.extend(run);
            }
            start = end;
        }
        out
    }

    /// The expected screen order: Arabic words read right to left, with
    /// mirrored brackets.
    fn screen(s: &str) -> String {
        let info = BidiInfo::new(s, Some(Level::rtl()));
        let para = &info.paragraphs[0];
        let (_, runs) = info.visual_runs(para, para.range.clone());
        let mut out = String::new();
        for run in runs {
            let chars = s[run.clone()].chars();
            if info.levels[run.start].is_rtl() { out.extend(chars.rev().map(mirror)) } else { out.extend(chars) }
        }
        out
    }

    #[test]
    fn latin_text_is_untouched() {
        assert_eq!(visual("Recover 12 files"), "Recover 12 files");
    }

    #[test]
    fn arabic_with_numbers_and_names_comes_out_in_screen_order() {
        for s in [
            "مرحبا بالعالم",
            "تم العثور على 25 ملفًا",
            "جاهز لفحص disk.img الآن",
            "الحجم: 345.4 KiB",
            "التقدم (25%)",
            "فتح صورة قرص…",
        ] {
            assert_eq!(egui_draws(&visual(s)), screen(s), "{s}");
        }
    }

    #[test]
    fn brackets_enclose_arabic_words() {
        // On screen: "(TRIM)" stays as is; around Arabic, "(" is on the left.
        let shown = egui_draws(&visual("تلقائيًا (TRIM)، لذا"));
        assert!(shown.contains("(TRIM)"), "{shown}");
        let shown = egui_draws(&visual("مثال (تجربة)"));
        assert!(shown.starts_with('(') && shown.contains(") "), "{shown}");
    }

    #[test]
    fn numbers_keep_their_digit_order() {
        // "25 files": on screen the number stays "25", to the right of the word.
        let shown = egui_draws(&visual("25 ملفًا"));
        assert!(shown.ends_with("25"), "{shown}");
    }

    const SOURCES: &[&str] = &[
        include_str!("app.rs"),
        include_str!("elevate.rs"),
        include_str!("home.rs"),
        include_str!("i18n.rs"),
        include_str!("jobs.rs"),
        include_str!("preview.rs"),
        include_str!("results.rs"),
        include_str!("settings.rs"),
        include_str!("theme.rs"),
        include_str!("views.rs"),
    ];

    /// Reads the string literal at the start of `s`: its value and its length
    /// in the source.
    fn literal(s: &str) -> Option<(String, usize)> {
        let mut out = String::new();
        let mut chars = s.strip_prefix('"')?.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => return Some((out, i + 2)),
                '\\' => out.push(chars.next()?.1),
                c => out.push(c),
            }
        }
        None
    }

    /// Every English string passed to the translation functions.
    fn keys() -> Vec<String> {
        let mut keys = Vec::new();
        for src in SOURCES {
            // Not the tests below.
            let src = src.split("#[cfg(test)]").next().unwrap_or_default();
            // (function, how many string arguments, whether the first argument is one)
            for (name, wanted, first) in [
                ("tr(", 1, true),
                ("trl(", 1, true),
                ("trf(", 1, true),
                ("trlf(", 1, true),
                ("trn(", 2, false),
                ("icon_label(", 1, false),
            ] {
                for (at, _) in src.match_indices(name) {
                    let before = &src[..at];
                    if before.ends_with(|c: char| c.is_alphanumeric() || c == '_') || before.ends_with("fn ") {
                        continue;
                    }
                    let mut rest = &src[at + name.len()..];
                    if first && !rest.trim_start().starts_with('"') {
                        continue;
                    }
                    for _ in 0..wanted {
                        let Some(q) = rest.find('"') else { break };
                        // Stay within the call.
                        if rest[..q].contains(';') {
                            break;
                        }
                        let Some((l, len)) = literal(&rest[q..]) else { break };
                        rest = &rest[q + len..];
                        keys.push(l);
                    }
                }
            }
        }
        keys.retain(|k| !k.is_empty());
        keys.sort();
        keys.dedup();
        keys
    }

    fn placeholders(s: &str) -> Vec<&str> {
        let mut v: Vec<&str> =
            s.match_indices('{').filter_map(|(i, _)| s[i..].find('}').map(|j| &s[i..i + j + 1])).collect();
        v.sort();
        v
    }

    #[test]
    fn every_string_is_translated() {
        let keys = keys();
        assert!(keys.len() > 100, "found only {} strings", keys.len());
        for (name, table) in [("German", translations::GERMAN), ("Arabic", translations::ARABIC)] {
            let map: HashMap<&str, &str> = table.iter().copied().collect();
            assert_eq!(map.len(), table.len(), "{name}: duplicate entries");
            let missing: Vec<&String> = keys.iter().filter(|k| !map.contains_key(k.as_str())).collect();
            assert!(missing.is_empty(), "{name}: missing translations:\n{missing:#?}");
            let unused: Vec<&&str> = map.keys().filter(|k| !keys.iter().any(|x| x == **k)).collect();
            assert!(unused.is_empty(), "{name}: translations of strings no longer used:\n{unused:#?}");
            for (en, t) in table {
                assert_eq!(placeholders(en), placeholders(t), "{name}: placeholders differ for {en:?}");
            }
        }
    }

    #[test]
    fn arabic_translations_use_only_letters_the_font_has() {
        for (en, t) in translations::ARABIC {
            for c in t.chars() {
                assert!(!is_arabic(c) || in_arabic_font(c), "{c:?} in the translation of {en:?}");
            }
        }
    }

    #[test]
    fn the_arabic_font_covers_neutral_characters() {
        // Spaces and digits must come from the Arabic font, or sentences
        // would be cut into separately shaped words.
        for c in " 0123456789:.,-".chars() {
            assert!(in_arabic_font(c), "{c:?}");
        }
        assert!(!in_arabic_font('a'));
    }
}
