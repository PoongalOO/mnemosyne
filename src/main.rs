//! Mnemosyne — lecteur vidéo minimaliste (H.264 / H.265) basé sur FFmpeg.
//!
//! Conteneurs : MP4, WebM, AVI, MKV. Utilisation :
//!   mnemosyne [--no-hw] [fichier]
//! On peut aussi ouvrir un fichier via le bouton « Ouvrir » ou le glisser
//! dans la fenêtre. Raccourcis : Espace = lecture/pause, ←/→ = ∓5 s,
//! ,/. = image précédente/suivante, ↑/↓ = volume, M = couper le son,
//! I/O = début/fin de sélection, Suppr = supprimer la sélection,
//! Ctrl+Z = annuler.

mod audio;
mod decoder;
mod edit;
mod export;
mod hwaccel;
mod playback;

use std::path::{Path, PathBuf};
use std::time::Duration;

use decoder::Decoder;
use eframe::egui;
use edit::Editor;

const EXTENSIONS: &[&str] = &["mp4", "m4v", "webm", "avi", "mkv"];
const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f32 = 0.05;

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
        }
        if pressed(egui::Key::O) {
            player.editor.mark_out = Some(player.position());
        }
        if pressed(egui::Key::Delete) || pressed(egui::Key::Backspace) {
            player.editor.cut_selection(duration);
        }
        if command && pressed(egui::Key::Z) {
            player.editor.undo();
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut toggle = false;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Ouvrir…").clicked() {
                self.open_dialog(&ctx);
            }

            let Some(player) = &mut self.player else {
                ui.label("Aucune vidéo — ouvrez un fichier ou glissez-le dans la fenêtre.");
                return;
            };

            let playing = !player.paused() && !player.finished();
            if ui.button(if playing { "⏸ Pause" } else { "▶ Lecture" }).clicked() {
                toggle = true;
            }
            if ui.button("⏹ Stop").clicked() {
                player.stop();
            }

            let duration = player.duration();
            let position = player.position();
            ui.label(format!("{} / {}", format_time_precise(position), format_time_precise(duration)));

            // Volume, aligné à droite ; la barre de progression prend le reste.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if player.decoder.has_audio() {
                    let (mut volume, muted) = player.decoder.playback.volume();
                    ui.spacing_mut().slider_width = 80.0;
                    if ui.add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false)).changed() {
                        player.decoder.playback.set_volume(volume, false);
                    }
                    let icon = if muted || volume == 0.0 { "🔇" } else { "🔊" };
                    if ui.button(icon).on_hover_text("Couper / rétablir le son (M)").clicked() {
                        player.toggle_mute();
                    }
                    ui.separator();
                }
                if duration > 0.0
                    && let Some(target) = timeline(ui, duration, position, &player.editor)
                {
                    player.seek(target);
                }
            });
        });
        if toggle {
            self.toggle_pause();
        }

        self.montage_controls(ui);

        let mut reopen = false;
        if let Some(player) = &self.player {
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
                        .checkbox(&mut self.hardware, "Accélération matérielle")
                        .on_hover_text("Décodage par le GPU (VA-API, NVDEC, VDPAU…)")
                        .changed();
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(text).weak()).truncate());
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
            ui.strong("Montage");
            if ui.button("Début [I]").on_hover_text("Début de la sélection à la position actuelle (touche I)").clicked() {
                editor.mark_in = Some(position);
            }
            if ui.button("Fin [O]").on_hover_text("Fin de la sélection à la position actuelle (touche O)").clicked() {
                editor.mark_out = Some(position);
            }
            let selection = editor.selection();
            if ui
                .add_enabled(selection.is_some(), egui::Button::new("Supprimer la sélection"))
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
            if ui.add_enabled(editor.can_undo(), egui::Button::new("Annuler")).on_hover_text("Ctrl+Z").clicked() {
                editor.undo();
            }
            if ui
                .add_enabled(!editor.cuts().is_empty() || selection.is_some(), egui::Button::new("Tout rétablir"))
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
            if ui
                .add_enabled(kept > 0.0, egui::Button::new("Enregistrer…"))
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

        egui::Panel::bottom("controles").show(ui, |ui| self.controls(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ui, |ui| {
                let area = ui.max_rect();
                let Some((texture, aspect)) = self
                    .player
                    .as_ref()
                    .and_then(|p| Some((p.texture.as_ref()?, p.decoder.info.display_aspect)))
                else {
                    return;
                };
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
            });
    }
}

/// Frise de lecture : passages supprimés (rouge), sélection (jaune) et
/// position. Renvoie la position demandée par un clic ou un glissement.
fn timeline(ui: &mut egui::Ui, duration: f64, position: f64, editor: &Editor) -> Option<f64> {
    let size = egui::vec2(ui.available_width().max(40.0), 22.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let x = |t: f64| rect.left() + ((t / duration).clamp(0.0, 1.0) as f32) * rect.width();
    let bar = rect.shrink2(egui::vec2(0.0, 6.0));

    painter.rect_filled(bar, 3.0, visuals.extreme_bg_color);
    let played = egui::Rect::from_min_max(bar.min, egui::pos2(x(position), bar.max.y));
    painter.rect_filled(played, 3.0, visuals.selection.bg_fill.gamma_multiply(0.6));

    let removed_color = egui::Color32::from_rgb(200, 60, 60);
    for &(a, b) in editor.cuts().removed() {
        let r = egui::Rect::from_x_y_ranges(x(a)..=x(b).max(x(a) + 1.0), bar.y_range());
        painter.rect_filled(r, 0.0, removed_color.gamma_multiply(0.9));
    }

    let mark_color = visuals.warn_fg_color;
    if let Some((a, b)) = editor.selection() {
        let r = egui::Rect::from_x_y_ranges(x(a)..=x(b), rect.y_range());
        painter.rect_filled(r, 0.0, mark_color.gamma_multiply(0.25));
    }
    for mark in [editor.mark_in, editor.mark_out].into_iter().flatten() {
        let mx = x(mark);
        painter.line_segment([egui::pos2(mx, rect.top()), egui::pos2(mx, rect.bottom())], (2.0, mark_color));
    }

    let px = x(position);
    let head = visuals.strong_text_color();
    painter.line_segment([egui::pos2(px, rect.top() + 2.0), egui::pos2(px, rect.bottom() - 2.0)], (2.0, head));
    painter.circle_filled(egui::pos2(px, rect.center().y), 4.0, head);

    let to_time = |p: egui::Pos2| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * duration;
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
