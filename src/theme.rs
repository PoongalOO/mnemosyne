//! Apparence : thèmes sombre et clair, couleur d'accent, police Inter et
//! icônes Phosphor intégrées (voir assets/LICENSE-*.txt), icône de la fenêtre.

use std::sync::Arc;

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals, vec2};

use crate::settings::ThemeChoice;

pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0x8c, 0xff);
/// Passages supprimés sur la frise.
pub const REMOVED: Color32 = Color32::from_rgb(0xe0, 0x4f, 0x5f);
/// Sélection du montage (début / fin).
pub const SELECTION: Color32 = Color32::from_rgb(0xf5, 0xc2, 0x42);

/// Points de code des icônes utilisées (police Phosphor Regular).
pub mod icon {
    pub const FOLDER_OPEN: &str = "\u{E256}";
    pub const FOLDER: &str = "\u{E24A}";
    pub const PLAY: &str = "\u{E3D0}";
    pub const PAUSE: &str = "\u{E39E}";
    pub const STOP: &str = "\u{E46C}";
    pub const REWIND: &str = "\u{E6A8}";
    pub const FAST_FORWARD: &str = "\u{E6A6}";
    pub const CARET_LEFT: &str = "\u{E138}";
    pub const CARET_RIGHT: &str = "\u{E13A}";
    pub const SPEAKER_HIGH: &str = "\u{E44A}";
    pub const SPEAKER_LOW: &str = "\u{E44C}";
    pub const SPEAKER_X: &str = "\u{E45C}";
    pub const CORNERS_OUT: &str = "\u{E1D0}";
    pub const CORNERS_IN: &str = "\u{E1CE}";
    pub const SCISSORS: &str = "\u{EAE0}";
    pub const FILM_STRIP: &str = "\u{E792}";
    pub const FILE_VIDEO: &str = "\u{EA22}";
    pub const ARROW_LINE_LEFT: &str = "\u{E062}";
    pub const ARROW_LINE_RIGHT: &str = "\u{E064}";
    pub const SKIP_BACK: &str = "\u{E5A4}";
    pub const SKIP_FORWARD: &str = "\u{E5A6}";
    pub const TRASH: &str = "\u{E4A6}";
    pub const ARROW_U_UP_LEFT: &str = "\u{E08A}";
    pub const ARROWS_CLOCKWISE: &str = "\u{E094}";
    pub const FLOPPY_DISK: &str = "\u{E248}";
    pub const UPLOAD: &str = "\u{E4BE}";
    pub const GEAR: &str = "\u{E270}";
    pub const INFO: &str = "\u{E2CE}";
    pub const AT: &str = "\u{E0AC}";
    pub const ENVELOPE: &str = "\u{E214}";
    pub const USER: &str = "\u{E4C2}";
    pub const LIST_BULLETS: &str = "\u{E2F2}";
    pub const KEYBOARD: &str = "\u{E2D8}";
    pub const CLOCK_COUNTER_CLOCKWISE: &str = "\u{E1A0}";
    pub const MAGNIFYING_GLASS_MINUS: &str = "\u{E30E}";
    pub const CROSSHAIR: &str = "\u{E1D6}";
    pub const LIGHTNING: &str = "\u{E2DE}";
    pub const TARGET: &str = "\u{E47C}";
    pub const SUN: &str = "\u{E472}";
    pub const MOON: &str = "\u{E330}";
    pub const MONITOR: &str = "\u{E32E}";
    pub const X: &str = "\u{E4F6}";
    pub const CHECK: &str = "\u{E182}";
}

const INTER: &[u8] = include_bytes!("../assets/Inter-Regular.ttf");
/// Réduite aux points de code des icônes (zone privée) : placée avant Inter
/// sans masquer le texte, et sans que les glyphes privés d'Inter masquent
/// les icônes.
const PHOSPHOR: &[u8] = include_bytes!("../assets/Phosphor.ttf");

pub fn install(ctx: &egui::Context, choice: ThemeChoice) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("inter".into(), egui::FontData::from_static(INTER).into());
    fonts.font_data.insert("phosphor".into(), egui::FontData::from_static(PHOSPHOR).into());
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, "inter".into());
    proportional.insert(0, "phosphor".into());
    // Icônes aussi dans le texte à chasse fixe (après la police de base).
    let monospace = fonts.families.entry(FontFamily::Monospace).or_default();
    monospace.insert(monospace.len().min(1), "phosphor".into());
    ctx.set_fonts(fonts);

    ctx.style_mut_of(egui::Theme::Dark, |style| configure(style, true));
    ctx.style_mut_of(egui::Theme::Light, |style| configure(style, false));
    apply(ctx, choice);
}

pub fn apply(ctx: &egui::Context, choice: ThemeChoice) {
    ctx.set_theme(match choice {
        ThemeChoice::Dark => egui::ThemePreference::Dark,
        ThemeChoice::Light => egui::ThemePreference::Light,
        ThemeChoice::System => egui::ThemePreference::System,
    });
}

fn configure(style: &mut egui::Style, dark: bool) {
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

    let rgb = Color32::from_rgb;
    // (fond des panneaux, fenêtres, discret, extrême, bordure, widget, survol, actif)
    let p = if dark {
        [rgb(0x17, 0x19, 0x1e), rgb(0x1f, 0x22, 0x29), rgb(0x1c, 0x1f, 0x25), rgb(0x0e, 0x10, 0x13),
         rgb(0x2c, 0x30, 0x38), rgb(0x26, 0x2a, 0x31), rgb(0x31, 0x36, 0x3f), rgb(0x3a, 0x40, 0x4b)]
    } else {
        [rgb(0xf3, 0xf4, 0xf6), rgb(0xff, 0xff, 0xff), rgb(0xea, 0xec, 0xef), rgb(0xe4, 0xe7, 0xeb),
         rgb(0xd3, 0xd7, 0xdd), rgb(0xe3, 0xe6, 0xea), rgb(0xd8, 0xdc, 0xe2), rgb(0xcb, 0xd0, 0xd8)]
    };
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p[0];
    v.window_fill = p[1];
    v.faint_bg_color = p[2];
    v.extreme_bg_color = p[3];
    v.window_stroke = Stroke::new(1.0, p[4]);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = if dark { SELECTION } else { rgb(0xa1, 0x62, 0x07) };

    let w = &mut v.widgets;
    for state in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        state.corner_radius = CornerRadius::same(6);
    }
    w.noninteractive.bg_stroke = Stroke::new(1.0, p[4]);
    w.inactive.bg_fill = p[5];
    w.inactive.weak_bg_fill = p[5];
    w.hovered.bg_fill = p[6];
    w.hovered.weak_bg_fill = p[6];
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
    w.active.bg_fill = p[7];
    w.active.weak_bg_fill = p[7];
    style.visuals = v;
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

/// Icône de la fenêtre : carré arrondi bleu et triangle de lecture blanc,
/// dessinés avec anticrénelage.
pub fn app_icon() -> Arc<egui::IconData> {
    const N: usize = 128;
    let mut rgba = vec![0u8; N * N * 4];
    let size = N as f32;
    let (radius, margin) = (size * 0.22, size * 0.04);
    // Triangle (centré optiquement, un peu à droite).
    let tri = [(0.38 * size, 0.28 * size), (0.38 * size, 0.72 * size), (0.74 * size, 0.5 * size)];
    let edge = |(ax, ay): (f32, f32), (bx, by): (f32, f32), x: f32, y: f32| {
        // Distance signée au bord (positive à l'intérieur).
        let (dx, dy) = (bx - ax, by - ay);
        ((x - ax) * dy - (y - ay) * dx) / (dx * dx + dy * dy).sqrt()
    };
    for py in 0..N {
        for px in 0..N {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            // Carré arrondi : distance signée.
            let (hx, hy) = (size / 2.0 - margin - radius, size / 2.0 - margin - radius);
            let (qx, qy) = ((x - size / 2.0).abs() - hx, (y - size / 2.0).abs() - hy);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - radius;
            let square = (0.5 - outside).clamp(0.0, 1.0);
            let inside_tri = [edge(tri[0], tri[1], x, y), edge(tri[1], tri[2], x, y), edge(tri[2], tri[0], x, y)]
                .into_iter()
                .fold(f32::INFINITY, f32::min);
            let white = (inside_tri + 0.5).clamp(0.0, 1.0);
            let (r, g, b) = (ACCENT.r() as f32, ACCENT.g() as f32, ACCENT.b() as f32);
            let mix = |c: f32| c + (255.0 - c) * white;
            let i = (py * N + px) * 4;
            rgba[i] = mix(r) as u8;
            rgba[i + 1] = mix(g) as u8;
            rgba[i + 2] = mix(b) as u8;
            rgba[i + 3] = (square * 255.0) as u8;
        }
    }
    Arc::new(egui::IconData { rgba, width: N as u32, height: N as u32 })
}
