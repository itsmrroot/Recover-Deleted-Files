//! Look and feel: palettes, fonts, egui style, and small reusable widgets.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, InnerResponse, Margin, Response,
    RichText, Sense, Shadow, Stroke, TextStyle, Theme, Ui, Vec2, Visuals,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::i18n;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Accent {
    Blue,
    Violet,
    Emerald,
    Orange,
    Rose,
}

impl Accent {
    pub const ALL: [Accent; 5] = [Accent::Blue, Accent::Violet, Accent::Emerald, Accent::Orange, Accent::Rose];

    pub fn color(self) -> Color32 {
        match self {
            Accent::Blue => Color32::from_rgb(59, 130, 246),
            Accent::Violet => Color32::from_rgb(139, 92, 246),
            Accent::Emerald => Color32::from_rgb(16, 185, 129),
            Accent::Orange => Color32::from_rgb(249, 115, 22),
            Accent::Rose => Color32::from_rgb(244, 63, 94),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Accent::Blue => i18n::tr("Blue"),
            Accent::Violet => i18n::tr("Violet"),
            Accent::Emerald => i18n::tr("Emerald"),
            Accent::Orange => i18n::tr("Orange"),
            Accent::Rose => i18n::tr("Rose"),
        }
    }
}

/// Colours for the current theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub sidebar: Color32,
    pub card: Color32,
    pub card_alt: Color32,
    pub border: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub deep: Color32,
    /// The top bar of the Midnight theme.
    pub header: Option<Color32>,
}

impl Palette {
    /// `midnight` only applies to the dark palette.
    pub fn new(dark: bool, midnight: bool, accent: Accent) -> Self {
        if dark && midnight {
            // Near-black surfaces under a deep blue top bar.
            Self {
                dark,
                bg: Color32::from_rgb(11, 12, 14),
                sidebar: Color32::from_rgb(17, 18, 21),
                card: Color32::from_rgb(22, 23, 27),
                card_alt: Color32::from_rgb(30, 32, 37),
                border: Color32::from_rgb(42, 45, 52),
                text: Color32::from_rgb(236, 238, 242),
                weak: Color32::from_rgb(148, 153, 165),
                accent: accent.color(),
                success: Color32::from_rgb(34, 197, 94),
                warning: Color32::from_rgb(245, 158, 11),
                danger: Color32::from_rgb(239, 68, 68),
                deep: Color32::from_rgb(167, 139, 250),
                header: Some(Color32::from_rgb(14, 40, 120)),
            }
        } else if dark {
            Self {
                dark,
                bg: Color32::from_rgb(15, 17, 23),
                sidebar: Color32::from_rgb(19, 22, 30),
                card: Color32::from_rgb(24, 28, 38),
                card_alt: Color32::from_rgb(31, 36, 48),
                border: Color32::from_rgb(41, 47, 62),
                text: Color32::from_rgb(230, 233, 239),
                weak: Color32::from_rgb(139, 147, 167),
                accent: accent.color(),
                success: Color32::from_rgb(34, 197, 94),
                warning: Color32::from_rgb(245, 158, 11),
                danger: Color32::from_rgb(239, 68, 68),
                deep: Color32::from_rgb(167, 139, 250),
                header: None,
            }
        } else {
            Self {
                dark,
                bg: Color32::from_rgb(244, 246, 250),
                sidebar: Color32::from_rgb(255, 255, 255),
                card: Color32::from_rgb(255, 255, 255),
                card_alt: Color32::from_rgb(241, 244, 249),
                border: Color32::from_rgb(226, 231, 239),
                text: Color32::from_rgb(17, 24, 39),
                weak: Color32::from_rgb(100, 110, 128),
                accent: accent.color(),
                success: Color32::from_rgb(22, 163, 74),
                warning: Color32::from_rgb(217, 119, 6),
                danger: Color32::from_rgb(220, 38, 38),
                deep: Color32::from_rgb(124, 58, 237),
                header: None,
            }
        }
    }

    /// A soft background tint of `c` that works on cards.
    pub fn tint(&self, c: Color32) -> Color32 {
        c.gamma_multiply(if self.dark { 0.16 } else { 0.10 })
    }
}

pub const SEMIBOLD: &str = "semibold";

fn semibold_family() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}

/// System UI font (Segoe UI on Windows, SF on macOS), the bundled Arabic and
/// Chinese fonts and Phosphor icons. While the interface is Arabic, the
/// Arabic font comes first so that spaces and digits in Arabic sentences use
/// it too (see `i18n::visual`); otherwise it is only a fallback for Arabic
/// file names. The bundled Chinese font only has the characters of the
/// translation, so a Chinese interface also loads the system's Chinese font
/// for file names.
pub fn install_fonts(ctx: &egui::Context, lang: i18n::Lang) {
    let mut fonts = FontDefinitions::default();
    let candidates: &[(&str, &str)] = if cfg!(windows) {
        &[("system", r"C:\Windows\Fonts\segoeui.ttf"), ("system-semibold", r"C:\Windows\Fonts\seguisb.ttf")]
    } else if cfg!(target_os = "macos") {
        &[("system", "/System/Library/Fonts/SFNS.ttf")]
    } else {
        &[]
    };
    for (name, bytes) in [
        ("arabic", i18n::ARABIC_FONT),
        ("arabic-semibold", i18n::ARABIC_FONT_SEMIBOLD),
        ("chinese", i18n::CHINESE_FONT),
        ("chinese-semibold", i18n::CHINESE_FONT_SEMIBOLD),
    ] {
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    let system_chinese = if lang == i18n::Lang::Zh { system_chinese_font() } else { None };
    if let Some(data) = system_chinese {
        fonts.font_data.insert("chinese-system".into(), Arc::new(data));
    }
    let mut loaded = Vec::new();
    for (name, path) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert((*name).to_string(), Arc::new(FontData::from_owned(bytes)));
            loaded.push(*name);
        }
    }
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    if loaded.contains(&"system") {
        proportional.insert(0, "system".into());
    }
    let mut semibold = proportional.clone();
    if loaded.contains(&"system-semibold") {
        semibold.insert(0, "system-semibold".into());
    }
    // Icons are inserted right after the text font of each family.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    semibold.insert(semibold.len().min(1), "phosphor".into());
    // Added after the icons, which must stay right behind the text font.
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    if lang == i18n::Lang::Ar {
        proportional.insert(0, "arabic".into());
        semibold.insert(0, "arabic-semibold".into());
    } else {
        proportional.push("arabic".into());
        semibold.push("arabic-semibold".into());
    }
    proportional.push("chinese".into());
    semibold.push("chinese-semibold".into());
    let monospace = fonts.families.entry(FontFamily::Monospace).or_default();
    monospace.extend(["arabic".into(), "chinese".into()]);
    if fonts.font_data.contains_key("chinese-system") {
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push("chinese-system".into());
        }
        semibold.push("chinese-system".into());
    }
    fonts.families.insert(semibold_family(), semibold);
    ctx.set_fonts(fonts);
}

/// The system's own Chinese font (tens of MB), for characters that the
/// bundled subset lacks, e.g. in Chinese file names.
fn system_chinese_font() -> Option<FontData> {
    let candidates: &[&str] = if cfg!(windows) {
        &[r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\simsun.ttc"]
    } else if cfg!(target_os = "macos") {
        &["/System/Library/Fonts/Hiragino Sans GB.ttc", "/System/Library/Fonts/STHeiti Medium.ttc"]
    } else {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        ]
    };
    candidates.iter().find_map(|p| std::fs::read(p).ok()).map(FontData::from_owned)
}

pub fn apply_style(ctx: &egui::Context, accent: Accent, midnight: bool) {
    for theme in [Theme::Dark, Theme::Light] {
        let pal = Palette::new(theme == Theme::Dark, midnight, accent);
        ctx.style_mut_of(theme, |s| {
            s.visuals = visuals(&pal);
            s.spacing.item_spacing = Vec2::new(8.0, 8.0);
            s.spacing.button_padding = Vec2::new(12.0, 6.0);
            s.spacing.interact_size.y = 30.0;
            s.spacing.window_margin = Margin::same(16);
            s.text_styles = [
                (TextStyle::Heading, FontId::new(24.0, semibold_family())),
                (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(14.5, FontFamily::Proportional)),
                (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
                (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
            ]
            .into();
        });
    }
}

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    let r = CornerRadius::same(8);
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(14);
    v.window_shadow =
        Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if p.dark { 110 } else { 40 }) };
    v.popup_shadow =
        Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(if p.dark { 90 } else { 30 }) };
    v.menu_corner_radius = r;
    v.extreme_bg_color = if p.header.is_some() {
        Color32::from_rgb(6, 7, 8)
    } else if p.dark {
        Color32::from_rgb(12, 14, 19)
    } else {
        Color32::WHITE
    };
    v.faint_bg_color = p.card_alt;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = blend(p.card, p.accent, if p.dark { 0.38 } else { 0.22 });
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.override_text_color = Some(p.text);
    v.weak_text_color = Some(p.weak);
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.striped = true;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.bg_fill = p.card_alt;
    w.inactive.weak_bg_fill = p.card_alt;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.bg_fill = p.card_alt;
    w.hovered.weak_bg_fill = if p.header.is_some() {
        Color32::from_rgb(38, 40, 46)
    } else if p.dark {
        Color32::from_rgb(40, 46, 60)
    } else {
        Color32::from_rgb(232, 236, 244)
    };
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.7));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.active.bg_fill = p.accent.gamma_multiply(0.35);
    w.active.weak_bg_fill = p.accent.gamma_multiply(0.35);
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.open = w.active;
    for s in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        s.corner_radius = r;
        s.expansion = 0.0;
    }
    v
}

/// Opaque mix of `a` towards `b` by `t`.
fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

// ---------------------------------------------------------------------------
// Widgets

pub fn semibold(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).family(semibold_family()).size(size)
}

pub fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(18))
        .show(ui, add)
}

pub fn page_title(ui: &mut Ui, p: &Palette, title: &str, subtitle: &str) {
    ui.label(semibold(title, 26.0).color(p.text));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(p.weak).size(15.0));
    }
    ui.add_space(14.0);
}

pub fn section_title(ui: &mut Ui, p: &Palette, icon: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(18.0).color(p.accent));
        ui.label(semibold(title, 16.5).color(p.text));
    });
    ui.add_space(4.0);
}

pub fn primary_button(ui: &mut Ui, p: &Palette, text: &str, enabled: bool) -> Response {
    let fill = if enabled { p.accent } else { p.accent.gamma_multiply(0.4) };
    ui.add_enabled(
        enabled,
        egui::Button::new(semibold(text, 15.0).color(Color32::WHITE))
            .fill(fill)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(10))
            .min_size(Vec2::new(0.0, 42.0)),
    )
}

pub fn secondary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(14.5))
            .corner_radius(CornerRadius::same(10))
            .min_size(Vec2::new(0.0, 36.0)),
    )
}

pub fn danger_button(ui: &mut Ui, p: &Palette, text: &str) -> Response {
    ui.add(
        egui::Button::new(semibold(text, 14.5).color(p.danger))
            .fill(p.tint(p.danger))
            .stroke(Stroke::new(1.0, p.danger.gamma_multiply(0.5)))
            .corner_radius(CornerRadius::same(10))
            .min_size(Vec2::new(0.0, 38.0)),
    )
}

/// Small rounded status label.
pub fn pill(ui: &mut Ui, p: &Palette, text: &str, color: Color32) -> Response {
    Frame::new()
        .fill(p.tint(color))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(9, 2))
        .show(ui, |ui| ui.label(RichText::new(text).size(12.0).color(color).strong()))
        .response
}

/// A clickable card; highlighted when `selected` or hovered.
pub fn selectable_card<R>(
    ui: &mut Ui,
    p: &Palette,
    id: egui::Id,
    selected: bool,
    enabled: bool,
    add: impl FnOnce(&mut Ui) -> R,
) -> Response {
    let hovered = enabled && ui.ctx().read_response(id).is_some_and(|r| r.hovered());
    let (fill, stroke) = if selected {
        (p.tint(p.accent), Stroke::new(1.5, p.accent))
    } else if hovered {
        (p.card_alt, Stroke::new(1.0, p.accent.gamma_multiply(0.6)))
    } else {
        (p.card, Stroke::new(1.0, p.border))
    };
    let inner = Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            if !enabled {
                ui.disable();
            }
            add(ui)
        });
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(inner.response.rect, id, sense);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// A big label/value pair for statistics.
pub fn stat(ui: &mut Ui, p: &Palette, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).size(12.5).color(p.weak));
        ui.label(semibold(value, 18.0).color(p.text));
    });
}

/// A notice box with an icon, e.g. a warning. `text` is in logical order
/// (see [`paragraph`]).
pub fn notice(ui: &mut Ui, p: &Palette, color: Color32, icon: &str, text: &str) {
    Frame::new()
        .fill(p.tint(color))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).size(18.0).color(color));
                paragraph(ui, text, 14.5, p.text);
            });
        });
}

/// Text wrapped to the available width. `text` is in logical order
/// (`i18n::trl`); right-to-left text is broken into lines here, because egui
/// would put the lines of reordered text in the wrong order. The lines line
/// up with the rest of the (left-to-right) layout.
pub fn paragraph(ui: &mut Ui, text: &str, size: f32, color: Color32) {
    if !i18n::is_rtl() {
        ui.add(egui::Label::new(RichText::new(text).size(size).color(color)).wrap());
        return;
    }
    let width = ui.available_width();
    let font = FontId::proportional(size);
    let measure = |ui: &Ui, s: &str| {
        let shown = i18n::visual(s).into_owned();
        ui.ctx().fonts_mut(|f| f.layout_no_wrap(shown, font.clone(), color).size().x)
    };
    let mut lines: Vec<String> = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && measure(ui, &candidate) > width {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            } else {
                line = candidate;
            }
        }
        lines.push(line);
    }
    let centered = ui.layout().is_vertical() && ui.layout().horizontal_align() == egui::Align::Center;
    let align = if centered { egui::Align::Center } else { egui::Align::Min };
    ui.with_layout(egui::Layout::top_down(align), |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for line in lines {
            ui.label(RichText::new(i18n::visual(&line)).size(size).color(color));
        }
    });
}
