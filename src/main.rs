//! Mnemosyne — lecteur vidéo minimaliste (H.264 / H.265) basé sur FFmpeg.
//!
//! Conteneurs : MP4, WebM, AVI, MKV. Utilisation :
//!   mnemosyne [--no-hw] [fichier]
//! On peut aussi ouvrir un fichier via le bouton « Ouvrir » ou le glisser
//! dans la fenêtre. Raccourcis : Espace = lecture/pause, ←/→ = ∓5 s,
//! ,/. = image précédente/suivante, ↑/↓ = volume, M = couper le son,
//! I/O = début/fin de sélection, Suppr = supprimer la sélection,
//! Ctrl+Z = annuler, F ou double-clic = plein écran, Échap = quitter le
//! plein écran.

mod audio;
mod decoder;
mod edit;
mod export;
mod hwaccel;
mod playback;
mod theme;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use decoder::Decoder;
use eframe::egui;
use edit::Editor;
use theme::{ACCENT, icon};

const EXTENSIONS: &[&str] = &["mp4", "m4v", "webm", "avi", "mkv"];
const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f32 = 0.05;
/// En plein écran, délai d'inactivité avant de masquer les contrôles.
const HIDE_CONTROLS_AFTER: Duration = Duration::from_millis(2500);

fn main() -> eframe::Result {
    let mut initial = None;
    let mut hardware = true;
    for arg in std::env::args_os().skip(1) {
        if arg == "--no-hw" {
            hardware = false;
        } else {
            initial = Some(PathBuf::from(arg));
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Mnemosyne")
            .with_inner_size([960.0, 600.0])
            .with_min_inner_size([480.0, 320.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Mnemosyne",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            let mut app = App { hardware, preview_cuts: true, ..Default::default() };
            if let Some(path) = initial {
                app.open(&cc.egui_ctx, path);
            }
            Ok(Box::new(app))
        }),
    )
}

struct Player {
    path: PathBuf,
    decoder: Decoder,
    texture: Option<egui::TextureHandle>,
    generation: u64,
    /// Vrai juste après une ouverture ou un déplacement : la prochaine image
    /// est affichée dès qu'elle arrive.
    awaiting_frame: bool,
    /// La dernière image affichée a été décodée par le GPU.
    last_frame_hardware: Option<bool>,
    /// Montage en cours sur cette vidéo.
    editor: Editor,
    /// Images clés (pour estimer le résultat du mode d'enregistrement rapide).
    keyframes: KeyframeScan,
}

/// Recensement des images clés, fait en arrière-plan à la demande.
#[derive(Default)]
enum KeyframeScan {
    #[default]
    NotStarted,
    Running(std::sync::mpsc::Receiver<Option<Vec<f64>>>),
    Done(Option<Vec<f64>>),
}

impl Player {
    fn new(path: PathBuf, hardware: bool) -> anyhow::Result<Self> {
        let decoder = Decoder::open(&path, hardware)?;
        Ok(Self {
            path,
            decoder,
            texture: None,
            generation: 0,
            awaiting_frame: true,
            last_frame_hardware: None,
            editor: Editor::default(),
            keyframes: KeyframeScan::NotStarted,
        })
    }

    fn duration(&self) -> f64 {
        self.decoder.info.duration
    }

    fn position(&self) -> f64 {
        let now = self.decoder.playback.now().max(0.0);
        if self.duration() > 0.0 { now.min(self.duration()) } else { now }
    }

    fn paused(&self) -> bool {
        self.decoder.playback.is_paused()
    }

    fn finished(&self) -> bool {
        self.decoder.playback.finished()
    }

    fn seek(&mut self, target: f64) {
        let duration = self.duration();
        let target = if duration > 0.0 { target.clamp(0.0, duration) } else { target.max(0.0) };
        self.generation += 1;
        self.decoder.playback.reset(target, self.generation);
        self.decoder.seek(target, self.generation);
        self.awaiting_frame = true;
    }

    fn toggle_pause(&mut self) {
        if self.finished() {
            // Relance depuis le début une fois la vidéo terminée.
            self.seek(0.0);
            self.decoder.playback.set_paused(false);
        } else {
            self.decoder.playback.set_paused(!self.paused());
        }
    }

    fn stop(&mut self) {
        self.seek(0.0);
        self.decoder.playback.set_paused(true);
    }

    /// Lance le recensement des images clés si besoin et renvoie le résultat
    /// quand il est disponible (`Some(None)` en cas d'échec).
    fn keyframes(&mut self) -> Option<Option<&[f64]>> {
        match &self.keyframes {
            KeyframeScan::NotStarted => {
                let (tx, rx) = std::sync::mpsc::channel();
                let path = self.path.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(export::keyframes(&path).ok());
                });
                self.keyframes = KeyframeScan::Running(rx);
            }
            KeyframeScan::Running(rx) => {
                if let Ok(result) = rx.try_recv() {
                    self.keyframes = KeyframeScan::Done(result);
                }
            }
            KeyframeScan::Done(_) => {}
        }
        match &self.keyframes {
            KeyframeScan::Done(keys) => Some(keys.as_deref()),
            _ => None,
        }
    }

    /// Avance ou recule d'une image, en pause.
    fn step_frame(&mut self, direction: f64) {
        let fps = self.decoder.info.fps;
        let step = if fps > 0.0 { 1.0 / fps } else { 1.0 / 25.0 };
        let target = self.position() + direction * step;
        self.seek(target);
        self.decoder.playback.set_paused(true);
    }

    fn change_volume(&self, delta: f32) {
        let (volume, _) = self.decoder.playback.volume();
        self.decoder.playback.set_volume(volume + delta, false);
    }

    fn toggle_mute(&self) {
        let (volume, muted) = self.decoder.playback.volume();
        self.decoder.playback.set_volume(volume, !muted);
    }

    /// Affiche l'image dont l'heure est venue. Renvoie une éventuelle erreur
    /// du décodeur.
    fn update(&mut self, ctx: &egui::Context) -> Option<String> {
        let playback = &self.decoder.playback;

        if let Some(frame) = playback.take_frame(self.awaiting_frame) {
            if self.awaiting_frame {
                self.awaiting_frame = false;
                playback.set_clock(frame.pts);
            }
            self.last_frame_hardware = Some(frame.hardware);
            let image = frame.image;
            match &mut self.texture {
                Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR))
                }
            }
        }
        if self.awaiting_frame && playback.end_of_stream() && !playback.has_frames() {
            self.awaiting_frame = false; // rien à afficher (déplacement en fin de fichier)
        }

        if playback.finished() && !playback.is_paused() {
            playback.set_paused(true);
        }

        // Planifie le prochain rafraîchissement.
        if self.awaiting_frame {
            ctx.request_repaint_after(Duration::from_millis(5));
        } else if !playback.is_paused() {
            let wait = playback.time_to_next_frame().unwrap_or(0.01).min(0.05);
            ctx.request_repaint_after(Duration::from_secs_f64(wait));
        }

        self.decoder.try_error()
    }

    fn decoding_label(&self) -> String {
        match (self.decoder.info.hwaccel, self.last_frame_hardware) {
            (Some(api), Some(true) | None) => format!("matériel ({api})"),
            (Some(api), Some(false)) => format!("logiciel (repli, {api} indisponible pour ce flux)"),
            (None, _) => "logiciel".to_owned(),
        }
    }
}

#[derive(Default)]
struct App {
    player: Option<Player>,
    error: Option<String>,
    /// Utiliser le décodage matériel quand il est disponible.
    hardware: bool,
    /// Sauter les passages supprimés pendant la lecture.
    preview_cuts: bool,
    export: ExportState,
    /// Fichier à ouvrir après la fenêtre d'enregistrement.
    pending_open: Option<PathBuf>,
    /// Barre d'outils de montage affichée.
    show_montage: bool,
    /// Dernier mouvement de souris ou appui de touche (plein écran).
    last_activity: Option<Instant>,
    /// Souris au-dessus des contrôles flottants du plein écran.
    controls_hovered: bool,
}

/// Fenêtre d'enregistrement de la vidéo montée.
#[derive(Default)]
enum ExportState {
    #[default]
    Closed,
    Setup {
        destination: PathBuf,
        mode: export::Mode,
        /// Remplacement d'un fichier existant confirmé dans le sélecteur.
        overwrite: bool,
        error: Option<String>,
    },
    Running(export::Export),
    Finished(Result<PathBuf, String>),
}

impl App {
    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        match Player::new(path, self.hardware) {
            Ok(player) => {
                let name = player
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} — Mnemosyne")));
                let info = &player.decoder.info;
                let mut warnings = Vec::new();
                if !info.supported_codec {
                    warnings.push(format!(
                        "Codec « {} » : seuls H.264 et H.265 sont officiellement pris en charge.",
                        info.codec_name
                    ));
                }
                warnings.extend(info.audio_error.clone());
                self.error = (!warnings.is_empty()).then(|| warnings.join("  "));
                self.player = Some(player);
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Rouvre le fichier courant (changement de mode de décodage) en
    /// conservant la position, l'état de pause et le volume.
    fn reopen(&mut self, ctx: &egui::Context) {
        let Some(old) = self.player.take() else { return };
        let (position, paused) = (old.position(), old.paused());
        let (volume, muted) = old.decoder.playback.volume();
        let path = old.path.clone();
        drop(old);
        self.open(ctx, path);
        if let Some(player) = &mut self.player {
            player.decoder.playback.set_volume(volume, muted);
            player.seek(position);
            player.decoder.playback.set_paused(paused);
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Ouvrir une vidéo")
            .add_filter("Vidéos (mp4, webm, avi, mkv)", EXTENSIONS)
            .add_filter("Tous les fichiers", &["*"]);
        if let Some(dir) = self.player.as_ref().and_then(|p| p.path.parent()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.open(ctx, path);
        }
    }

    /// Lecture / pause. Avec l'aperçu du montage, relancer la lecture à la
    /// fin du dernier passage conservé repart du premier.
    fn toggle_pause(&mut self) {
        let Some(player) = &mut self.player else { return };
        if self.preview_cuts && player.paused() {
            let kept = player.editor.cuts().kept(player.duration());
            if let (Some(first), Some(last)) = (kept.first(), kept.last())
                && player.position() >= last.1 - 0.05
            {
                player.seek(first.0);
                player.decoder.playback.set_paused(false);
                return;
            }
        }
        player.toggle_pause();
    }

    /// Pendant la lecture, saute les passages supprimés (aperçu du montage).
    fn skip_removed(&mut self) {
        let Some(player) = &mut self.player else { return };
        if !self.preview_cuts || player.paused() || player.awaiting_frame {
            return;
        }
        let duration = player.duration();
        if let Some((start, end)) = player.editor.cuts().removed_at(player.position()) {
            if end >= duration - 1e-3 {
                // Fin supprimée : on s'arrête sur la dernière image conservée.
                player.seek((start - 1e-3).max(0.0));
                player.decoder.playback.set_paused(true);
            } else {
                player.seek(end);
            }
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped {
            self.open(ctx, path);
        }

        if self.player.is_none() || ctx.egui_wants_keyboard_input() {
            return;
        }
        let pressed = |key| ctx.input(|i| i.key_pressed(key));
        let command = ctx.input(|i| i.modifiers.command);

        if pressed(egui::Key::Space) {
            self.toggle_pause();
        }
        let Some(player) = &mut self.player else { return };
        let duration = player.duration();
        if pressed(egui::Key::ArrowLeft) {
            player.seek(player.position() - SEEK_STEP);
        }
        if pressed(egui::Key::ArrowRight) {
            player.seek(player.position() + SEEK_STEP);
        }
        if pressed(egui::Key::Comma) {
            player.step_frame(-1.0);
        }
        if pressed(egui::Key::Period) {
            player.step_frame(1.0);
        }
        if pressed(egui::Key::ArrowUp) {
            player.change_volume(VOLUME_STEP);
        }
        if pressed(egui::Key::ArrowDown) {
            player.change_volume(-VOLUME_STEP);
        }
        if pressed(egui::Key::M) {
            player.toggle_mute();
        }
        if pressed(egui::Key::I) {
            player.editor.mark_in = Some(player.position());
            self.show_montage = true;
        }
        if pressed(egui::Key::O) {
            player.editor.mark_out = Some(player.position());
            self.show_montage = true;
        }
        if pressed(egui::Key::Delete) || pressed(egui::Key::Backspace) {
            player.editor.cut_selection(duration);
        }
        if command && pressed(egui::Key::Z) {
            player.editor.undo();
        }
        if pressed(egui::Key::F) {
            toggle_fullscreen(ctx);
        }
        if pressed(egui::Key::Escape) && is_fullscreen(ctx) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
    }

    /// Contrôles de lecture : frise pleine largeur, boutons, montage et
    /// informations. `overlay` : version flottante du plein écran.
    fn controls(&mut self, ui: &mut egui::Ui, overlay: bool) {
        let ctx = ui.ctx().clone();
        let Some(player) = &mut self.player else { return };
        let (mut toggle, mut open) = (false, false);
        let duration = player.duration();
        let position = player.position();

        ui.add_space(4.0);
        if duration > 0.0
            && let Some(target) = timeline(ui, duration, position, &player.editor)
        {
            player.seek(target);
        }

        ui.horizontal(|ui| {
            if theme::icon_button(ui, icon::FOLDER_OPEN, "Ouvrir une vidéo").clicked() {
                open = true;
            }
            ui.separator();
            let playing = !player.paused() && !player.finished();
            let (play_icon, play_tip) =
                if playing { (icon::PAUSE, "Pause (Espace)") } else { (icon::PLAY, "Lecture (Espace)") };
            if theme::accent_button(ui, play_icon, 20.0, egui::vec2(44.0, 30.0)).on_hover_text(play_tip).clicked() {
                toggle = true;
            }
            if theme::icon_button(ui, icon::STOP, "Stop").clicked() {
                player.stop();
            }
            ui.add_space(4.0);
            // Chiffres à chasse fixe : l'affichage ne bouge pas pendant la lecture.
            ui.label(egui::RichText::new(format_time_precise(position)).monospace().strong());
            ui.label(egui::RichText::new(format!("/ {}", format_time_precise(duration))).monospace().weak());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let fullscreen = is_fullscreen(&ctx);
                let (fs_icon, fs_tip) = if fullscreen {
                    (icon::CORNERS_IN, "Quitter le plein écran (F, Échap)")
                } else {
                    (icon::CORNERS_OUT, "Plein écran (F, double-clic)")
                };
                if theme::icon_button(ui, fs_icon, fs_tip).clicked() {
                    toggle_fullscreen(&ctx);
                }
                if duration > 0.0
                    && ui
                        .add(
                            egui::Button::new(format!("{}  Montage", icon::SCISSORS))
                                .selected(self.show_montage)
                                .min_size(egui::vec2(0.0, 30.0)),
                        )
                        .on_hover_text("Afficher / masquer les outils de montage")
                        .clicked()
                {
                    self.show_montage = !self.show_montage;
                }
                if player.decoder.has_audio() {
                    ui.separator();
                    let (mut volume, muted) = player.decoder.playback.volume();
                    ui.spacing_mut().slider_width = 90.0;
                    if ui
                        .add(
                            egui::Slider::new(&mut volume, 0.0..=1.0)
                                .show_value(false)
                                .handle_shape(egui::style::HandleShape::Circle),
                        ).changed() {
                        player.decoder.playback.set_volume(volume, false);
                    }
                    let speaker = if muted || volume == 0.0 {
                        icon::SPEAKER_X
                    } else if volume < 0.5 {
                        icon::SPEAKER_LOW
                    } else {
                        icon::SPEAKER_HIGH
                    };
                    if theme::icon_button(ui, speaker, "Couper / rétablir le son (M)").clicked() {
                        player.toggle_mute();
                    }
                }
            });
        });
        if toggle {
            self.toggle_pause();
        }
        if open {
            self.open_dialog(&ctx);
        }

        if self.show_montage {
            ui.add_space(2.0);
            ui.separator();
            self.montage_controls(ui);
        }

        let mut reopen = false;
        if let Some(player) = &self.player
            && !overlay
        {
            let info = &player.decoder.info;
            ui.horizontal(|ui| {
                let mut text = format!(
                    "{}  •  {}×{}  •  {}",
                    info.codec_name, info.width, info.height, info.container
                );
                if info.fps > 0.0 {
                    text += &format!("  •  {:.3} im/s", info.fps);
                }
                text += &format!("  •  décodage {}", player.decoding_label());
                if let Some(audio) = &info.audio {
                    text += &format!("  •  audio {audio}");
                }
                // Case à cocher à droite, informations tronquées dans l'espace restant.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    reopen = ui
                        .checkbox(&mut self.hardware, egui::RichText::new("Accélération matérielle").small())
                        .on_hover_text("Décodage par le GPU (VA-API, NVDEC, VDPAU…)")
                        .changed();
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(text).small().weak()).truncate());
                    });
                });
            });
        }
        if reopen {
            self.reopen(&ctx);
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().warn_fg_color, error);
        }
        ui.add_space(4.0);
    }

    /// Écran d'accueil, sans vidéo ouverte.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let area = ui.max_rect();
        let zone = egui::Rect::from_center_size(
            area.center(),
            egui::vec2((area.width() - 48.0).clamp(200.0, 560.0), (area.height() - 48.0).clamp(200.0, 340.0)),
        );
        let hovering = files_hovered(&ctx);
        let color = if hovering { ACCENT } else { ui.visuals().widgets.noninteractive.bg_stroke.color };
        if hovering {
            ui.painter().rect_filled(zone, 16.0, ACCENT.gamma_multiply(0.08));
        }
        dashed_rect(ui.painter(), zone, color);

        let mut open = false;
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(zone.shrink(16.0)).layout(egui::Layout::top_down(egui::Align::Center)),
            |ui| {
                let content = if self.error.is_some() { 270.0 } else { 240.0 };
                ui.add_space(((ui.available_height() - content) / 2.0).max(0.0));
                ui.label(egui::RichText::new(icon::FILM_STRIP).size(56.0).color(ACCENT));
                ui.label(egui::RichText::new("Mnemosyne").size(28.0).strong());
                ui.weak("Lecture et montage de vidéos H.264 / H.265");
                ui.add_space(14.0);
                if theme::accent_button(
                    ui,
                    format!("{}   Ouvrir une vidéo…", icon::FOLDER_OPEN),
                    16.0,
                    egui::vec2(230.0, 40.0),
                )
                .clicked()
                {
                    open = true;
                }
                ui.add_space(6.0);
                if hovering {
                    ui.label(egui::RichText::new(format!("{}  Déposez le fichier pour l'ouvrir", icon::UPLOAD)).color(ACCENT));
                } else {
                    ui.weak("ou glissez un fichier dans cette fenêtre");
                }
                ui.label(egui::RichText::new("MP4  ·  MKV  ·  WebM  ·  AVI").small().weak());
                if let Some(error) = &self.error {
                    ui.add_space(10.0);
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            },
        );
        if open {
            self.open_dialog(&ctx);
        }
    }
}

impl App {
    /// Barre d'outils de montage.
    fn montage_controls(&mut self, ui: &mut egui::Ui) {
        let Some(player) = &mut self.player else { return };
        let duration = player.duration();
        if duration <= 0.0 {
            return;
        }
        let position = player.position();
        let editor = &mut player.editor;
        let mut open_export = false;

        ui.horizontal_wrapped(|ui| {
            if ui.button(format!("{}  Début [I]", icon::ARROW_LINE_RIGHT)).on_hover_text("Début de la sélection à la position actuelle (touche I)").clicked() {
                editor.mark_in = Some(position);
            }
            if ui.button(format!("{}  Fin [O]", icon::ARROW_LINE_LEFT)).on_hover_text("Fin de la sélection à la position actuelle (touche O)").clicked() {
                editor.mark_out = Some(position);
            }
            let selection = editor.selection();
            if ui
                .add_enabled(selection.is_some(), egui::Button::new(format!("{}  Supprimer la sélection", icon::TRASH)))
                .on_hover_text("Supprime le passage entre le début et la fin de la sélection (touche Suppr)")
                .clicked()
            {
                editor.cut_selection(duration);
            }
            ui.separator();
            if ui.button("Couper avant").on_hover_text("Supprime tout ce qui précède la position actuelle").clicked() {
                editor.cut_before(position, duration);
            }
            if ui.button("Couper après").on_hover_text("Supprime tout ce qui suit la position actuelle").clicked() {
                editor.cut_after(position, duration);
            }
            ui.separator();
            if ui.add_enabled(editor.can_undo(), egui::Button::new(format!("{}  Annuler", icon::ARROW_U_UP_LEFT))).on_hover_text("Ctrl+Z").clicked() {
                editor.undo();
            }
            if ui
                .add_enabled(!editor.cuts().is_empty() || selection.is_some(), egui::Button::new(format!("{}  Tout rétablir", icon::ARROWS_CLOCKWISE)))
                .on_hover_text("Rétablit toute la vidéo et efface la sélection")
                .clicked()
            {
                editor.reset();
            }
            ui.checkbox(&mut self.preview_cuts, "Aperçu")
                .on_hover_text("Saute les passages supprimés pendant la lecture");
            ui.separator();

            let kept = editor.cuts().kept_duration(duration);
            // Étiquette insécable : jamais coupée lors du retour à la ligne.
            ui.add(
                egui::Label::new(format!("Durée finale : {}", format_time_precise(kept)))
                    .wrap_mode(egui::TextWrapMode::Extend),
            );
            let save = egui::Button::new(
                egui::RichText::new(format!("{}  Enregistrer…", icon::FLOPPY_DISK)).color(egui::Color32::WHITE),
            )
            .fill(ACCENT);
            if ui
                .add_enabled(kept > 0.0, save)
                .on_hover_text("Enregistre la vidéo montée dans un nouveau fichier")
                .clicked()
            {
                open_export = true;
            }
            if let Some((a, b)) = editor.selection() {
                ui.add(egui::Label::new(egui::RichText::new(format!(
                    "Sélection : {} → {} ({:.1} s)",
                    format_time_precise(a),
                    format_time_precise(b),
                    b - a
                )).weak()).wrap_mode(egui::TextWrapMode::Extend));
            } else if let Some(m) = editor.mark_in.or(editor.mark_out) {
                ui.weak(format!("Repère à {}", format_time_precise(m)));
            }
        });

        if open_export {
            player.decoder.playback.set_paused(true);
            self.export = ExportState::Setup {
                destination: default_destination(&player.path),
                mode: export::Mode::Precise,
                overwrite: false,
                error: None,
            };
        }
    }

    /// Fenêtre d'enregistrement (réglages, progression, résultat).
    fn export_window(&mut self, ctx: &egui::Context) {
        let Some(player) = &mut self.player else {
            self.export = ExportState::Closed;
            return;
        };
        let source = player.path.clone();
        let duration = player.duration();
        let segments = player.editor.cuts().kept(duration);
        let kept: f64 = segments.iter().map(|(s, e)| e - s).sum();
        // Résultat du mode rapide : `None` pendant l'analyse.
        let fast_estimate = player.keyframes().map(|keys| {
            keys.map(|keys| {
                export::fast_segments(&segments, keys, duration).iter().map(|(s, e)| e - s).sum::<f64>()
            })
        });
        let mut next = None;

        let response = egui::Modal::new(egui::Id::unique("enregistrement")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Enregistrer la vidéo montée");
            ui.add_space(6.0);
            match &mut self.export {
                ExportState::Closed => {}
                ExportState::Setup { destination, mode, overwrite, error } => {
                    ui.label(format!(
                        "Durée finale : {} ({} passage{} conservé{})",
                        format_time_precise(kept),
                        segments.len(),
                        if segments.len() > 1 { "s" } else { "" },
                        if segments.len() > 1 { "s" } else { "" },
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Fichier :");
                        ui.strong(destination.file_name().unwrap_or_default().to_string_lossy());
                        if ui.button("Choisir…").clicked()
                            && let Some(path) = save_dialog(&source, destination)
                        {
                            *destination = path;
                            *overwrite = true; // confirmé par le sélecteur
                            *error = None;
                        }
                    });
                    if let Some(dir) = destination.parent() {
                        ui.horizontal(|ui| {
                            ui.label("Dossier :");
                            ui.add(egui::Label::new(egui::RichText::new(dir.display().to_string()).weak()).truncate());
                        });
                    }
                    ui.add_space(6.0);
                    ui.radio_value(mode, export::Mode::Precise, "Précis (réencodage)")
                        .on_hover_text("Coupes à l'image près. Plus lent ; qualité quasi identique.");
                    ui.radio_value(mode, export::Mode::Fast, "Rapide (sans réencodage)")
                        .on_hover_text(
                            "Quasi instantané et sans perte, mais les coupes sont calées sur les \
                             images clés : elles peuvent être décalées de quelques secondes.",
                        );
                    if *mode == export::Mode::Fast {
                        ui.add_space(4.0);
                        match fast_estimate {
                            None => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.weak("Analyse des images clés…");
                                });
                                ctx.request_repaint_after(Duration::from_millis(100));
                            }
                            Some(None) => {
                                ui.weak("Impossible d'analyser les images clés.");
                            }
                            Some(Some(fast)) if (fast - kept).abs() < 0.05 => {
                                ui.weak("Les coupes tombent sur des images clés : résultat exact.");
                            }
                            Some(Some(fast)) if fast >= duration - 0.05 => {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    "Pas d'image clé aux endroits coupés : le mode rapide \
                                     enregistrerait la vidéo entière. Utilisez le mode précis.",
                                );
                            }
                            Some(Some(fast)) => {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    format!(
                                        "Coupes calées sur les images clés : durée obtenue {} au lieu de {}.",
                                        format_time_precise(fast),
                                        format_time_precise(kept)
                                    ),
                                );
                            }
                        }
                    }
                    if let Some(e) = error {
                        ui.add_space(4.0);
                        ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Enregistrer").clicked() {
                            if export::same_file(&source, destination) {
                                *error = Some("C'est le fichier d'origine : choisissez un autre nom.".into());
                            } else if destination.exists() && !*overwrite {
                                *error = Some("Ce fichier existe déjà : choisissez un autre nom.".into());
                            } else {
                                match export::Export::start(&source, destination, segments.clone(), *mode) {
                                    Ok(job) => next = Some(ExportState::Running(job)),
                                    Err(e) => *error = Some(format!("{e:#}")),
                                }
                            }
                        }
                        if ui.button("Annuler").clicked() {
                            next = Some(ExportState::Closed);
                        }
                    });
                }
                ExportState::Running(job) => {
                    ui.label(format!("Écriture de {}", job.destination.display()));
                    ui.add(egui::ProgressBar::new(job.progress()).show_percentage());
                    if ui.button("Annuler").clicked() {
                        job.cancel();
                    }
                    if let Some(result) = job.poll() {
                        next = Some(ExportState::Finished(result.map(|()| job.destination.clone())));
                    }
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                ExportState::Finished(result) => {
                    match result {
                        Ok(path) => {
                            ui.label("Vidéo enregistrée :");
                            ui.monospace(path.display().to_string());
                        }
                        Err(e) if e == export::CANCELLED => {
                            ui.label("Enregistrement annulé ; aucun fichier n'a été créé.");
                        }
                        Err(e) => {
                            ui.colored_label(ui.visuals().error_fg_color, format!("Échec : {e}"));
                        }
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if let Ok(path) = result
                            && ui.button("Ouvrir la vidéo enregistrée").clicked()
                        {
                            next = Some(ExportState::Finished(Ok(path.clone())));
                            self.pending_open = Some(path.clone());
                        }
                        if ui.button("Fermer").clicked() {
                            next = Some(ExportState::Closed);
                        }
                    });
                }
            }
        });

        // Échap / clic à côté : ferme, sauf pendant l'écriture.
        if response.should_close() && !matches!(self.export, ExportState::Running(_)) {
            next = Some(ExportState::Closed);
        }
        if let Some(state) = next {
            self.export = state;
        }
        if let Some(path) = self.pending_open.take() {
            self.export = ExportState::Closed;
            self.open(ctx, path);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let modal_open = !matches!(self.export, ExportState::Closed);
        if !modal_open {
            self.handle_input(&ctx);
        }

        if let Some(player) = &mut self.player
            && let Some(e) = player.update(&ctx)
        {
            self.error = Some(e);
        }
        self.skip_removed();
        if modal_open {
            self.export_window(&ctx);
        }

        // Plein écran : contrôles masqués après un moment sans activité.
        let fullscreen = is_fullscreen(&ctx);
        let active = ctx.input(|i| {
            i.pointer.delta() != egui::Vec2::ZERO
                || i.pointer.any_pressed()
                || i.events.iter().any(|e| matches!(e, egui::Event::Key { .. }))
        });
        let now = Instant::now();
        if active || self.last_activity.is_none() {
            self.last_activity = Some(now);
        }
        let idle = self.last_activity.map_or(Duration::ZERO, |t| now - t);
        let paused = self.player.as_ref().is_none_or(|p| p.paused());
        let show_controls =
            !fullscreen || paused || modal_open || self.controls_hovered || idle < HIDE_CONTROLS_AFTER;
        if fullscreen && !show_controls {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        } else if fullscreen && !paused {
            ctx.request_repaint_after(HIDE_CONTROLS_AFTER.saturating_sub(idle));
        }

        if self.player.is_some() && !fullscreen {
            egui::Panel::bottom("controles")
                .frame(egui::Frame::side_top_panel(&ctx.global_style()).inner_margin(egui::Margin::symmetric(12, 4)))
                .show(ui, |ui| self.controls(ui, false));
        }

        let background =
            if self.player.is_some() { egui::Color32::BLACK } else { ctx.global_style().visuals.extreme_bg_color };
        let mut double_clicked = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(background))
            .show(ui, |ui| {
                if self.player.is_none() {
                    self.welcome(ui);
                    return;
                }
                let area = ui.max_rect();
                double_clicked = ui.interact(area, egui::Id::unique("video"), egui::Sense::click()).double_clicked();
                if let Some((texture, aspect)) = self
                    .player
                    .as_ref()
                    .and_then(|p| Some((p.texture.as_ref()?, p.decoder.info.display_aspect)))
                {
                    // Image centrée, proportions conservées.
                    let mut size = egui::vec2(area.width(), area.width() / aspect);
                    if size.y > area.height() {
                        size = egui::vec2(area.height() * aspect, area.height());
                    }
                    let rect = egui::Rect::from_center_size(area.center(), size);
                    // Les images sont converties directement à la taille affichée.
                    if let Some(player) = &self.player {
                        let ppp = ctx.pixels_per_point();
                        player.decoder.playback.set_target_size((
                            (size.x * ppp).round() as u32,
                            (size.y * ppp).round() as u32,
                        ));
                    }
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    ui.painter().image(texture.id(), rect, uv, egui::Color32::WHITE);
                }
                if files_hovered(&ctx) {
                    let painter = ui.painter();
                    painter.rect_filled(area, 0.0, egui::Color32::from_black_alpha(170));
                    dashed_rect(painter, area.shrink(24.0), ACCENT);
                    painter.text(
                        area.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("{}  Déposez la vidéo pour l'ouvrir", icon::UPLOAD),
                        egui::FontId::proportional(22.0),
                        egui::Color32::WHITE,
                    );
                }
            });
        if double_clicked {
            toggle_fullscreen(&ctx);
        }

        // Contrôles flottants en bas de l'écran.
        self.controls_hovered = false;
        if fullscreen && show_controls && self.player.is_some() {
            let width = (ctx.content_rect().width() - 48.0).min(1200.0);
            let response = egui::Area::new(egui::Id::unique("controles_plein_ecran"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -24.0])
                .show(&ctx, |ui| {
                    egui::Frame::window(ui.style())
                        .fill(ui.visuals().panel_fill.gamma_multiply(0.92))
                        .inner_margin(egui::Margin::symmetric(16, 8))
                        .show(ui, |ui| {
                            ui.set_width(width - 32.0);
                            self.controls(ui, true);
                        });
                })
                .response;
            self.controls_hovered = response.contains_pointer();
        }
    }
}

fn is_fullscreen(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
}

fn toggle_fullscreen(ctx: &egui::Context) {
    let fullscreen = is_fullscreen(ctx);
    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
}

/// Un fichier est en train d'être glissé au-dessus de la fenêtre.
fn files_hovered(ctx: &egui::Context) -> bool {
    ctx.input(|i| !i.raw.hovered_files.is_empty())
}

/// Cadre en pointillés (zone de dépôt).
fn dashed_rect(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()];
    painter.extend(egui::Shape::dashed_line(&corners, egui::Stroke::new(1.5, color), 8.0, 6.0));
}

/// Frise de lecture : passages supprimés (rouge), sélection (jaune) et
/// position. Renvoie la position demandée par un clic ou un glissement.
fn timeline(ui: &mut egui::Ui, duration: f64, position: f64, editor: &Editor) -> Option<f64> {
    let size = egui::vec2(ui.available_width().max(40.0), 22.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    // Un peu de marge : la tête de lecture déborde aux extrémités.
    let painter = ui.painter_at(rect.expand2(egui::vec2(8.0, 0.0)));
    let x = |t: f64| rect.left() + ((t / duration).clamp(0.0, 1.0) as f32) * rect.width();
    // La barre s'épaissit au survol.
    let active = response.hovered() || response.dragged();
    let half = if active { 4.0 } else { 2.5 };
    let bar = egui::Rect::from_x_y_ranges(rect.x_range(), rect.center().y - half..=rect.center().y + half);

    painter.rect_filled(bar, half, egui::Color32::from_rgb(0x33, 0x38, 0x42));
    let played = egui::Rect::from_min_max(bar.min, egui::pos2(x(position), bar.max.y));
    painter.rect_filled(played, half, ACCENT);

    for &(a, b) in editor.cuts().removed() {
        let r = egui::Rect::from_x_y_ranges(x(a)..=x(b).max(x(a) + 1.0), bar.y_range());
        painter.rect_filled(r, 0.0, theme::REMOVED);
    }

    if let Some((a, b)) = editor.selection() {
        let r = egui::Rect::from_x_y_ranges(x(a)..=x(b), rect.y_range());
        painter.rect_filled(r, 2.0, theme::SELECTION.gamma_multiply(0.25));
    }
    for mark in [editor.mark_in, editor.mark_out].into_iter().flatten() {
        let mx = x(mark);
        painter.line_segment([egui::pos2(mx, rect.top()), egui::pos2(mx, rect.bottom())], (2.0, theme::SELECTION));
    }

    let to_time = |p: egui::Pos2| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * duration;
    if let Some(p) = response.hover_pos() {
        let hx = p.x.clamp(rect.left(), rect.right());
        painter.line_segment(
            [egui::pos2(hx, rect.top() + 3.0), egui::pos2(hx, rect.bottom() - 3.0)],
            (1.0, egui::Color32::from_white_alpha(90)),
        );
    }

    let head = egui::pos2(x(position), rect.center().y);
    painter.circle(head, if active { 7.0 } else { 5.5 }, egui::Color32::WHITE, (2.0, ACCENT));

    let target = if response.clicked() || response.dragged() {
        response.interact_pointer_pos().map(to_time)
    } else {
        None
    };
    if let Some(p) = response.hover_pos() {
        response.on_hover_text_at_pointer(format_time_precise(to_time(p)));
    }
    target
}

/// Nom proposé pour la vidéo montée, à côté de l'original, sans écraser de
/// fichier existant. Les WebM (H.264/H.265 non standard) deviennent des MKV.
fn default_destination(source: &Path) -> PathBuf {
    let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "video".into());
    let ext = match source.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some(e @ ("mp4" | "m4v" | "mov" | "avi" | "mkv")) => e.to_owned(),
        _ => "mkv".to_owned(),
    };
    let dir = source.parent().unwrap_or(Path::new("."));
    (1..)
        .map(|n| {
            let suffix = if n == 1 { String::new() } else { format!("-{n}") };
            dir.join(format!("{stem}_montage{suffix}.{ext}"))
        })
        .find(|p| !p.exists())
        .unwrap()
}

/// Sélecteur « Enregistrer sous ».
fn save_dialog(source: &Path, current: &Path) -> Option<PathBuf> {
    let filters: [(&str, &[&str]); 3] = [("MP4", &["mp4"]), ("Matroska (MKV)", &["mkv"]), ("AVI", &["avi"])];
    let current_ext = current.extension().and_then(|e| e.to_str()).unwrap_or("mkv").to_ascii_lowercase();
    let mut dialog = rfd::FileDialog::new().set_title("Enregistrer la vidéo montée");
    // Le format du nom proposé en premier.
    for (name, exts) in filters.iter().filter(|(_, e)| e.contains(&current_ext.as_str())) {
        dialog = dialog.add_filter(*name, exts);
    }
    for (name, exts) in filters.iter().filter(|(_, e)| !e.contains(&current_ext.as_str())) {
        dialog = dialog.add_filter(*name, exts);
    }
    if let Some(dir) = current.parent().or(source.parent()) {
        dialog = dialog.set_directory(dir);
    }
    if let Some(name) = current.file_name() {
        dialog = dialog.set_file_name(name.to_string_lossy());
    }
    let mut path = dialog.save_file()?;
    if path.extension().is_none() {
        path.set_extension(&current_ext);
    }
    Some(path)
}

/// Position avec les dixièmes de seconde (utile pour le montage).
fn format_time_precise(seconds: f64) -> String {
    let tenths = (seconds.max(0.0) * 10.0).round() as u64;
    format!("{}.{}", format_time((tenths / 10) as f64), tenths % 10)
}

fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}
