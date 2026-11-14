//! Colours and fonts of the AUBLst dispatch frontend (frontend/src/styles/app.css),
//! so the adapter looks like part of it.

use std::path::Path;

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, Stroke};

pub const BG: Color32 = Color32::from_rgb(16, 17, 19);
pub const CARD: Color32 = Color32::from_rgb(21, 22, 25);
pub const FIELD: Color32 = Color32::from_rgb(11, 12, 13);
pub const BOX: Color32 = Color32::from_rgb(23, 24, 27);
pub const BORDER: Color32 = Color32::from_rgb(42, 44, 48);
pub const BUTTON: Color32 = Color32::from_rgb(38, 40, 44);
pub const BUTTON_HOVER: Color32 = Color32::from_rgb(50, 53, 59);
pub const TEXT: Color32 = Color32::from_rgb(233, 234, 236);
pub const STRONG: Color32 = Color32::WHITE;
pub const DIM: Color32 = Color32::from_rgb(154, 157, 164);
pub const HINT: Color32 = Color32::from_rgb(92, 95, 101);
// Primary buttons and selected tabs are light with dark text, as in the frontend.
pub const PRIMARY: Color32 = Color32::from_rgb(233, 234, 236);
pub const ON_PRIMARY: Color32 = BG;
pub const LINK: Color32 = Color32::from_rgb(143, 182, 255);
pub const GOOD: Color32 = Color32::from_rgb(46, 201, 142);
pub const WARN: Color32 = Color32::from_rgb(240, 160, 60);
pub const BAD: Color32 = Color32::from_rgb(232, 82, 74);
pub const OFF: Color32 = Color32::from_rgb(90, 92, 97);
pub const DANGER_FILL: Color32 = Color32::from_rgb(128, 45, 41);
pub const WARN_FILL: Color32 = Color32::from_rgb(144, 96, 36);
pub const WARN_BG: Color32 = Color32::from_rgb(36, 30, 22);
pub const BAD_BG: Color32 = Color32::from_rgb(40, 22, 22);

/// Codepoints in assets/Phosphor.ttf, from phosphoricons.com.
pub mod icon {
    pub const CONNECT: &str = "\u{EB5A}";
    pub const DISCONNECT: &str = "\u{EB56}";
    pub const COPY: &str = "\u{E1CA}";
    pub const CHECK: &str = "\u{E182}";
    pub const LINK: &str = "\u{E2E2}";
    pub const OPEN: &str = "\u{E5DE}";
    pub const FOLDER: &str = "\u{E256}";
    pub const FILE: &str = "\u{E23A}";
    pub const PLAY: &str = "\u{E3D0}";
    pub const GOOD: &str = "\u{E184}";
    pub const INFO: &str = "\u{E06C}";
    pub const WARN: &str = "\u{E4E0}";
    pub const BAD: &str = "\u{E4E4}";
}

pub fn with_icon(icon: &str, text: &str) -> String {
    format!("{icon}  {text}")
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.faint_bg_color = CARD;
    v.extreme_bg_color = FIELD;
    v.text_edit_bg_color = Some(FIELD);
    v.hyperlink_color = LINK;
    v.weak_text_color = Some(DIM);
    v.selection.bg_fill = PRIMARY;
    v.selection.stroke = Stroke::new(1.0, ON_PRIMARY);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    for (w, fill) in [(&mut v.widgets.inactive, BUTTON), (&mut v.widgets.hovered, BUTTON_HOVER), (&mut v.widgets.active, BUTTON_HOVER)] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.fg_stroke = Stroke::new(1.0, TEXT);
    }
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v
}

/// Segoe UI as in the frontend, read from Windows at runtime so the exe doesn't carry
/// it; egui's own font stays as the fallback. The icon font comes after the text fonts.
pub fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("phosphor".into(), FontData::from_static(include_bytes!("../assets/Phosphor.ttf")).into());
    let windows = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let mut load = |name: &str, file: &str| {
        let bytes = std::fs::read(Path::new(&windows).join("Fonts").join(file)).ok()?;
        fonts.font_data.insert(name.into(), FontData::from_owned(bytes).into());
        Some(name.to_string())
    };
    let regular = load("segoe", "segoeui.ttf");
    let bold = load("segoe-semibold", "seguisb.ttf");

    let mut proportional = fonts.families[&FontFamily::Proportional].clone();
    proportional.splice(0..0, regular);
    proportional.push("phosphor".into());
    let mut strong = proportional.clone();
    strong.splice(0..0, bold);
    fonts.families.insert(FontFamily::Proportional, proportional);
    fonts.families.insert(semibold(), strong);
    fonts.families.get_mut(&FontFamily::Monospace).unwrap().push("phosphor".into());
    fonts
}
