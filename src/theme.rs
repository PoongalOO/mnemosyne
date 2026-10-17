//! Apparence : thème sombre, couleur d'accent et icônes Phosphor intégrées
//! (assets/Phosphor.ttf, licence MIT).

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, vec2};

pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0x8c, 0xff);
/// Passages supprimés sur la frise.
pub const REMOVED: Color32 = Color32::from_rgb(0xe0, 0x4f, 0x5f);
/// Sélection du montage (début / fin).
pub const SELECTION: Color32 = Color32::from_rgb(0xf5, 0xc2, 0x42);

/// Points de code des icônes utilisées (police Phosphor Regular).
pub mod icon {
    pub const FOLDER_OPEN: &str = "\u{E256}";
    pub const PLAY: &str = "\u{E3D0}";
    pub const PAUSE: &str = "\u{E39E}";
    pub const STOP: &str = "\u{E46C}";
    pub const SPEAKER_HIGH: &str = "\u{E44A}";
    pub const SPEAKER_LOW: &str = "\u{E44C}";
    pub const SPEAKER_X: &str = "\u{E45C}";
    pub const CORNERS_OUT: &str = "\u{E1D0}";
    pub const CORNERS_IN: &str = "\u{E1CE}";
    pub const SCISSORS: &str = "\u{EAE0}";
    pub const FILM_STRIP: &str = "\u{E792}";
    pub const ARROW_LINE_LEFT: &str = "\u{E062}";
    pub const ARROW_LINE_RIGHT: &str = "\u{E064}";
    pub const TRASH: &str = "\u{E4A6}";
    pub const ARROW_U_UP_LEFT: &str = "\u{E08A}";
    pub const ARROWS_CLOCKWISE: &str = "\u{E094}";
    pub const FLOPPY_DISK: &str = "\u{E248}";
    pub const UPLOAD: &str = "\u{E4BE}";
}

const PHOSPHOR: &[u8] = include_bytes!("../assets/Phosphor.ttf");

pub fn install(ctx: &egui::Context) {
    // Icônes en secours de la police de texte : on peut les mêler au texte.
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("phosphor".into(), egui::FontData::from_static(PHOSPHOR).into());
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(proportional.len().min(1), "phosphor".into());
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Heading, FontId::proportional(20.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();

        let spacing = &mut style.spacing;
        spacing.item_spacing = vec2(8.0, 6.0);
        spacing.button_padding = vec2(10.0, 5.0);
        spacing.interact_size.y = 26.0;

        let border = Color32::from_rgb(0x2c, 0x30, 0x38);
        let v = &mut style.visuals;
        v.panel_fill = Color32::from_rgb(0x17, 0x19, 0x1e);
        v.window_fill = Color32::from_rgb(0x1f, 0x22, 0x29);
        v.faint_bg_color = Color32::from_rgb(0x1c, 0x1f, 0x25);
        v.extreme_bg_color = Color32::from_rgb(0x0e, 0x10, 0x13);
        v.window_stroke = Stroke::new(1.0, border);
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(8);
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = SELECTION;

        let w = &mut v.widgets;
        for state in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            state.corner_radius = CornerRadius::same(6);
        }
        w.noninteractive.bg_stroke = Stroke::new(1.0, border);
        w.inactive.bg_fill = Color32::from_rgb(0x26, 0x2a, 0x31);
        w.inactive.weak_bg_fill = w.inactive.bg_fill;
        w.hovered.bg_fill = Color32::from_rgb(0x31, 0x36, 0x3f);
        w.hovered.weak_bg_fill = w.hovered.bg_fill;
        w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
        w.active.bg_fill = Color32::from_rgb(0x3a, 0x40, 0x4b);
        w.active.weak_bg_fill = w.active.bg_fill;
    });
}

/// Bouton d'icône seule, avec info-bulle.
pub fn icon_button(ui: &mut egui::Ui, icon: &str, tooltip: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(icon).size(18.0)).min_size(vec2(34.0, 30.0)))
        .on_hover_text(tooltip)
}

/// Bouton principal, sur fond de couleur d'accent.
pub fn accent_button(ui: &mut egui::Ui, text: impl Into<String>, size: f32, min: egui::Vec2) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(size).color(Color32::WHITE))
            .fill(ACCENT)
            .min_size(min),
    )
}
