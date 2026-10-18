//! Frise de lecture et de montage.
//!
//! - clic ou glissement : déplacement ;
//! - molette : zoom autour du pointeur, défilement horizontal : déplacement
//!   de la portion visible ;
//! - glisser un repère de sélection (jaune) le déplace ; Maj + glisser crée
//!   une sélection ;
//! - clic droit : menu (repères, rétablir un passage supprimé…).
//!
//! Avec les outils de montage affichés, la frise montre aussi les images clés
//! et la forme d'onde du son.

use eframe::egui::{self, Color32, Rect, Sense, pos2, vec2};

use crate::edit::Editor;
use crate::theme::{self, ACCENT, icon};
use crate::waveform::{self, Peaks};
use crate::{format_time, format_time_precise};

/// Plus petite portion visible, en secondes.
const MIN_SPAN: f64 = 1.0;
const BAR_HEIGHT: f32 = 22.0;
const WAVE_HEIGHT: f32 = 30.0;
const TICKS_HEIGHT: f32 = 14.0;
/// Distance (en points) à laquelle on attrape un repère.
const GRAB: f32 = 6.0;

#[derive(Default)]
pub struct State {
    /// Portion visible (début, fin) ; `None` : toute la vidéo.
    view: Option<(f64, f64)>,
    drag: Drag,
    /// Position du dernier clic droit.
    menu_time: f64,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Drag {
    #[default]
    None,
    Seek,
    MarkIn,
    MarkOut,
    Select,
}

pub struct Input<'a> {
    pub duration: f64,
    pub position: f64,
    pub playing: bool,
    /// Outils de montage affichés : images clés et forme d'onde.
    pub detailed: bool,
    pub keyframes: Option<&'a [f64]>,
    pub peaks: Option<&'a Peaks>,
}

#[derive(Default)]
pub struct Output {
    pub seek: Option<f64>,
    /// Message à afficher brièvement.
    pub message: Option<String>,
}

impl State {
    /// Facteur de zoom (`None` : toute la vidéo est visible).
    pub fn zoom(&self, duration: f64) -> Option<f64> {
        self.view.map(|(a, b)| duration / (b - a))
    }

    pub fn reset_zoom(&mut self) {
        self.view = None;
    }

    fn range(&self, duration: f64) -> (f64, f64) {
        self.view.unwrap_or((0.0, duration))
    }

    fn set_view(&mut self, start: f64, span: f64, duration: f64) {
        let span = span.clamp(MIN_SPAN.min(duration), duration);
        if span >= duration * 0.999 {
            self.view = None;
        } else {
            let start = start.clamp(0.0, duration - span);
            self.view = Some((start, start + span));
        }
    }

    /// Zoom de `factor` (> 1 : rapproche) en gardant `t` sous le pointeur.
    fn zoom_around(&mut self, t: f64, factor: f64, duration: f64) {
        let (a, b) = self.range(duration);
        let span = ((b - a) / factor).clamp(MIN_SPAN.min(duration), duration);
        let ratio = if b > a { (t - a) / (b - a) } else { 0.5 };
        self.set_view(t - ratio * span, span, duration);
    }
}

/// Affiche la frise. `hover_ui` remplit l'info-bulle de survol (position et
/// largeur de la portion visible en secondes).
pub fn show(
    ui: &mut egui::Ui,
    state: &mut State,
    editor: &mut Editor,
    input: &Input,
    hover_ui: &mut dyn FnMut(&mut egui::Ui, f64, f64),
) -> Output {
    let mut out = Output::default();
    let duration = input.duration;
    let has_wave = input.detailed && input.peaks.is_some_and(|p| !p.values.is_empty());
    let height = BAR_HEIGHT + if has_wave { WAVE_HEIGHT } else { 0.0 } + TICKS_HEIGHT;
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width().max(40.0), height), Sense::click_and_drag());
    let bar_rect = Rect::from_min_size(rect.min, vec2(rect.width(), BAR_HEIGHT));
    let wave_rect = Rect::from_min_size(pos2(rect.left(), bar_rect.bottom()), vec2(rect.width(), WAVE_HEIGHT));
    let ticks_rect = Rect::from_min_max(pos2(rect.left(), rect.bottom() - TICKS_HEIGHT), rect.max);
    let content_bottom = ticks_rect.top();

    // ---- Zoom et défilement ----
    if response.hovered()
        && let Some(p) = response.hover_pos()
    {
        let (a, b) = state.range(duration);
        let t = a + ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * (b - a);
        let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
        if zoom != 1.0 {
            state.zoom_around(t, f64::from(zoom), duration);
        } else if scroll.y != 0.0 {
            state.zoom_around(t, f64::from(scroll.y * 0.004).exp(), duration);
        }
        if scroll.x != 0.0 && state.view.is_some() {
            let (a, b) = state.range(duration);
            let shift = -f64::from(scroll.x / rect.width()) * (b - a);
            state.set_view(a + shift, b - a, duration);
        }
    }
    // Pendant la lecture, la portion visible suit la tête de lecture.
    if input.playing
        && state.drag == Drag::None
        && let Some((a, b)) = state.view
        && (input.position < a || input.position > b)
    {
        let span = b - a;
        state.set_view(input.position - 0.05 * span, span, duration);
    }

    let (a, b) = state.range(duration);
    let span = b - a;
    let x = |t: f64| rect.left() + ((t - a) / span) as f32 * rect.width();
    let to_time = |px: f32| a + ((px - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * span;

    // ---- Interaction ----
    let shift = ui.input(|i| i.modifiers.shift);
    let near = |mark: Option<f64>, px: f32| mark.is_some_and(|m| (x(m) - px).abs() <= GRAB);
    if let Some(p) = response.hover_pos()
        && (near(editor.mark_in, p.x) || near(editor.mark_out, p.x))
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    if response.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin()).or(response.interact_pointer_pos());
        if let Some(origin) = origin {
            state.drag = if shift {
                let t = to_time(origin.x);
                editor.mark_in = Some(t);
                editor.mark_out = Some(t);
                Drag::Select
            } else if near(editor.mark_in, origin.x) {
                Drag::MarkIn
            } else if near(editor.mark_out, origin.x) {
                Drag::MarkOut
            } else {
                Drag::Seek
            };
        }
    }
    if response.dragged()
        && let Some(p) = response.interact_pointer_pos()
    {
        let t = to_time(p.x);
        match state.drag {
            Drag::Seek => out.seek = Some(t),
            Drag::MarkIn => editor.mark_in = Some(t),
            Drag::MarkOut | Drag::Select => editor.mark_out = Some(t),
            Drag::None => {}
        }
    }
    if response.drag_stopped() {
        if matches!(state.drag, Drag::MarkIn | Drag::MarkOut | Drag::Select)
            && let Some((s, e)) = editor.selection()
        {
            out.message = Some(format!("Sélection : {} → {}", format_time_precise(s), format_time_precise(e)));
        }
        state.drag = Drag::None;
    }
    if response.clicked()
        && !shift
        && let Some(p) = response.interact_pointer_pos()
    {
        out.seek = Some(to_time(p.x));
    }
    if response.secondary_clicked()
        && let Some(p) = response.interact_pointer_pos()
    {
        state.menu_time = to_time(p.x);
    }

    // ---- Dessin ----
    let painter = ui.painter_at(rect.expand2(vec2(8.0, 0.0)));
    let visuals = ui.visuals();
    let dark = visuals.dark_mode;
    let weak = visuals.weak_text_color();
    let active = response.hovered() || response.dragged();
    let half = if active { 4.0 } else { 2.5 };
    let cy = bar_rect.center().y;
    let bar = Rect::from_x_y_ranges(rect.x_range(), cy - half..=cy + half);
    let track = if dark { Color32::from_rgb(0x33, 0x38, 0x42) } else { Color32::from_rgb(0xc9, 0xce, 0xd6) };
    let clip_x = |px: f32| px.clamp(rect.left(), rect.right());

    painter.rect_filled(bar, half, track);
    let played = Rect::from_x_y_ranges(rect.left()..=clip_x(x(input.position)), bar.y_range());
    painter.rect_filled(played, half, ACCENT);

    // Forme d'onde.
    if has_wave && let Some(peaks) = input.peaks {
        painter.rect_filled(wave_rect.shrink2(vec2(0.0, 2.0)), 4.0, visuals.faint_bg_color);
        let max = peaks.max.max(1e-3);
        let amplitude = WAVE_HEIGHT / 2.0 - 3.0;
        let color = if dark { Color32::from_rgb(0x8a, 0x93, 0xa3) } else { Color32::from_rgb(0x6b, 0x73, 0x80) };
        let columns = rect.width().floor() as usize;
        for col in 0..columns {
            let t0 = to_time(rect.left() + col as f32);
            let t1 = to_time(rect.left() + col as f32 + 1.0);
            let i0 = (t0 / waveform::BUCKET) as usize;
            let i1 = ((t1 / waveform::BUCKET).ceil() as usize).max(i0 + 1).min(peaks.values.len());
            if i0 >= i1 {
                continue;
            }
            let v = peaks.values[i0..i1].iter().copied().fold(0.0f32, f32::max);
            let h = (v / max).powf(0.7) * amplitude;
            if h >= 0.5 {
                let px = rect.left() + col as f32 + 0.5;
                let c = wave_rect.center().y;
                painter.line_segment([pos2(px, c - h), pos2(px, c + h)], (1.0, color));
            }
        }
    }

    // Passages supprimés.
    for &(s, e) in editor.cuts().removed() {
        if e < a || s > b {
            continue;
        }
        let xs = clip_x(x(s))..=clip_x(x(e)).max(clip_x(x(s)) + 1.0);
        painter.rect_filled(Rect::from_x_y_ranges(xs.clone(), bar.y_range()), 0.0, theme::REMOVED);
        if has_wave {
            painter.rect_filled(Rect::from_x_y_ranges(xs, wave_rect.y_range()), 0.0, theme::REMOVED.gamma_multiply(0.3));
        }
    }

    // Images clés.
    if input.detailed && let Some(keys) = input.keyframes {
        let visible: Vec<f64> = keys.iter().copied().filter(|&k| k >= a && k <= b).collect();
        if (visible.len() as f32) <= rect.width() / 4.0 {
            for k in visible {
                let px = x(k);
                painter.line_segment([pos2(px, bar.bottom() + 2.0), pos2(px, bar.bottom() + 6.0)], (1.0, weak));
            }
        }
    }

    // Sélection et repères (avec une poignée en haut).
    if let Some((s, e)) = editor.selection() {
        let r = Rect::from_x_y_ranges(clip_x(x(s))..=clip_x(x(e)), rect.top()..=content_bottom);
        painter.rect_filled(r, 2.0, theme::SELECTION.gamma_multiply(0.22));
    }
    for mark in [editor.mark_in, editor.mark_out].into_iter().flatten() {
        if mark < a || mark > b {
            continue;
        }
        let mx = x(mark);
        painter.line_segment([pos2(mx, rect.top()), pos2(mx, content_bottom)], (2.0, theme::SELECTION));
        painter.rect_filled(Rect::from_center_size(pos2(mx, rect.top() + 4.0), vec2(8.0, 8.0)), 2.0, theme::SELECTION);
    }

    // Graduations.
    let step = tick_step(span, rect.width());
    let mut t = (a / step).ceil() * step;
    let font = egui::FontId::proportional(10.0);
    while t <= b + 1e-9 {
        let px = x(t);
        painter.line_segment([pos2(px, ticks_rect.top()), pos2(px, ticks_rect.top() + 3.0)], (1.0, weak));
        let label = if step < 1.0 { format_time_precise(t) } else { format_time(t) };
        let align = if px - rect.left() < 20.0 {
            egui::Align2::LEFT_TOP
        } else if rect.right() - px < 20.0 {
            egui::Align2::RIGHT_TOP
        } else {
            egui::Align2::CENTER_TOP
        };
        painter.text(pos2(px, ticks_rect.top() + 2.0), align, label, font.clone(), weak);
        t += step;
    }

    // Aperçu de la portion visible quand on a zoomé.
    if state.view.is_some() {
        let overview = Rect::from_x_y_ranges(rect.x_range(), rect.top()..=rect.top() + 2.0);
        painter.rect_filled(overview, 1.0, track);
        let left = rect.left() + (a / duration) as f32 * rect.width();
        let right = rect.left() + (b / duration) as f32 * rect.width();
        painter.rect_filled(Rect::from_x_y_ranges(left..=right.max(left + 2.0), overview.y_range()), 1.0, ACCENT);
    }

    // Survol et tête de lecture.
    if let Some(p) = response.hover_pos() {
        let hx = clip_x(p.x);
        let line = if dark { Color32::from_white_alpha(90) } else { Color32::from_black_alpha(90) };
        painter.line_segment([pos2(hx, rect.top() + 3.0), pos2(hx, content_bottom - 3.0)], (1.0, line));
    }
    if input.position >= a && input.position <= b {
        let px = x(input.position);
        if has_wave {
            painter.line_segment([pos2(px, bar.bottom()), pos2(px, content_bottom)], (1.0, ACCENT));
        }
        painter.circle(pos2(px, cy), if active { 7.0 } else { 5.5 }, Color32::WHITE, (2.0, ACCENT));
    }

    // ---- Menu contextuel ----
    let t = state.menu_time;
    response.context_menu(|ui| {
        ui.label(egui::RichText::new(format_time_precise(t)).monospace().weak());
        ui.separator();
        if ui.button(format!("{}  Aller ici", icon::CROSSHAIR)).clicked() {
            out.seek = Some(t);
            ui.close();
        }
        if ui.button(format!("{}  Début de la sélection ici", icon::ARROW_LINE_RIGHT)).clicked() {
            editor.mark_in = Some(t);
            ui.close();
        }
        if ui.button(format!("{}  Fin de la sélection ici", icon::ARROW_LINE_LEFT)).clicked() {
            editor.mark_out = Some(t);
            ui.close();
        }
        if editor.selection().is_some()
            && ui.button(format!("{}  Supprimer la sélection", icon::TRASH)).clicked()
        {
            editor.cut_selection(duration);
            out.message = Some("Passage supprimé".into());
            ui.close();
        }
        if let Some((s, e)) = editor.cuts().removed_at(t)
            && ui.button(format!("{}  Rétablir ce passage", icon::ARROWS_CLOCKWISE)).clicked()
        {
            editor.restore(s, e);
            out.message = Some("Passage rétabli".into());
            ui.close();
        }
        if state.view.is_some() && ui.button(format!("{}  Tout afficher", icon::MAGNIFYING_GLASS_MINUS)).clicked() {
            state.view = None;
            ui.close();
        }
    });

    if state.drag == Drag::None
        && let Some(p) = response.hover_pos()
    {
        let t = to_time(p.x);
        response.on_hover_ui_at_pointer(|ui| hover_ui(ui, t, span));
    }
    out
}

/// Intervalle entre graduations : au moins 70 points entre deux étiquettes.
fn tick_step(span: f64, width: f32) -> f64 {
    const STEPS: &[f64] = &[
        0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0,
    ];
    let wanted = span * 70.0 / f64::from(width.max(1.0));
    STEPS.iter().copied().find(|&s| s >= wanted).unwrap_or(14400.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graduations() {
        // 100 s sur 700 points : au moins 10 s entre deux étiquettes.
        assert_eq!(tick_step(100.0, 700.0), 10.0);
        assert_eq!(tick_step(3.0, 700.0), 0.5);
        assert_eq!(tick_step(7200.0, 900.0), 600.0);
    }

    #[test]
    fn zoom_et_bornes() {
        let mut state = State::default();
        state.zoom_around(50.0, 4.0, 100.0);
        assert_eq!(state.view, Some((37.5, 62.5))); // 50 reste au même endroit
        state.zoom_around(37.5, 1000.0, 100.0);
        assert_eq!(state.view, Some((37.5, 38.5))); // portion minimale : 1 s
        state.set_view(-20.0, 10.0, 100.0);
        assert_eq!(state.view, Some((0.0, 10.0))); // pas avant le début
        state.set_view(95.0, 10.0, 100.0);
        assert_eq!(state.view, Some((90.0, 100.0))); // ni après la fin
        state.zoom_around(50.0, 0.01, 100.0);
        assert_eq!(state.view, None); // dézoom complet : toute la vidéo
        assert_eq!(state.zoom(100.0), None);
    }
}
