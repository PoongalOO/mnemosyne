//! Mnemosyne — lecteur vidéo minimaliste (H.264 / H.265) basé sur FFmpeg.
//!
//! Conteneurs : MP4, WebM, AVI, MKV. Utilisation :
//!   mnemosyne [--no-hw] [fichier]
//! On peut aussi ouvrir un fichier depuis l'écran d'accueil (bouton
//! « Ouvrir », fichiers récents) ou le glisser dans la fenêtre. Les
//! raccourcis clavier sont listés dans [`SHORTCUTS`] (touche F1).

mod audio;
mod decoder;
mod edit;
mod export;
mod hwaccel;
mod playback;
mod settings;
mod theme;
mod thumbnails;
mod timeline;
mod waveform;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use decoder::Decoder;
use eframe::egui;
use edit::Editor;
use settings::{Settings, ThemeChoice};
use theme::{ACCENT, icon};
use thumbnails::Thumbnailer;
use waveform::Waveform;

const EXTENSIONS: &[&str] = &["mp4", "m4v", "webm", "avi", "mkv"];
const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f32 = 0.05;
/// En plein écran, délai d'inactivité avant de masquer les contrôles.
const HIDE_CONTROLS_AFTER: Duration = Duration::from_millis(2500);
/// Durée d'affichage des messages en surimpression.
const OSD_DURATION: Duration = Duration::from_millis(1400);
/// Durée de l'icône lecture / pause affichée au centre de l'image.
const FLASH_DURATION: Duration = Duration::from_millis(600);
/// Délai avant de traiter un clic sur l'image, pour le distinguer d'un
/// double-clic (plein écran).
const CLICK_DELAY: Duration = Duration::from_millis(300);
/// Miniatures de survol gardées en mémoire.
const THUMB_CACHE: usize = 96;
/// Fichiers récents affichés sur l'écran d'accueil.
const RECENT_SHOWN: usize = 5;

/// Raccourcis clavier et souris (fenêtre d'aide).
const SHORTCUTS: &[(&str, &str)] = &[
    ("Espace, clic sur l'image", "Lecture / pause"),
    ("← / →", "Reculer / avancer de 5 s"),
    (", / .", "Image précédente / suivante"),
    ("↑ / ↓", "Volume"),
    ("M", "Couper / rétablir le son"),
    ("F, double-clic", "Plein écran"),
    ("Échap", "Quitter le plein écran"),
    ("I / O", "Début / fin de la sélection"),
    ("Maj+I / Maj+O", "Aller au début / à la fin de la sélection"),
    ("Suppr", "Supprimer la sélection"),
    ("Ctrl+Z", "Annuler la dernière coupe"),
    ("Molette sur la frise", "Zoom de la frise"),
    ("Maj + glisser sur la frise", "Créer une sélection"),
    ("Glisser un repère jaune", "Déplacer le début / la fin"),
    ("Clic droit sur la frise", "Repères, rétablir un passage…"),
    ("Ctrl+O", "Ouvrir une vidéo"),
    ("F1, ?", "Cette aide"),
];

fn main() -> eframe::Result {
    let mut initial = None;
    let mut no_hw = false;
    for arg in std::env::args_os().skip(1) {
        if arg == "--no-hw" {
            no_hw = true;
        } else {
            initial = Some(PathBuf::from(arg));
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Mnemosyne")
            .with_inner_size([960.0, 600.0])
            .with_min_inner_size([480.0, 320.0])
            .with_drag_and_drop(true)
            .with_icon(theme::app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Mnemosyne",
        options,
        Box::new(move |cc| {
            let settings: Settings =
                cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
            theme::install(&cc.egui_ctx, settings.theme);
            // `--no-hw` ne vaut que pour cette session.
            let mut app = App { hardware: settings.hardware && !no_hw, settings, ..Default::default() };
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
    /// Images clés (repères de la frise, estimation du mode rapide).
    keyframes: KeyframeScan,
    /// Zoom de la frise et glissement en cours.
    timeline: timeline::State,
    /// Miniatures de survol de la frise.
    thumbs: Thumbs,
    /// Forme d'onde, calculée quand les outils de montage sont affichés.
    waveform: Option<Waveform>,
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
            timeline: timeline::State::default(),
            thumbs: Thumbs::default(),
            waveform: None,
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

    /// Affiche l'image dont l'heure est venue. Renvoie une éventuelle erreur
    /// du décodeur.
    fn update(&mut self, ctx: &egui::Context) -> Option<String> {
        self.thumbs.poll(ctx);
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

/// Miniatures de survol : produites à la demande, gardées en cache.
#[derive(Default)]
struct Thumbs {
    /// Décodeur dédié, lancé au premier survol.
    worker: Option<Thumbnailer>,
    /// Par position (en millisecondes).
    cache: HashMap<i64, egui::TextureHandle>,
    order: VecDeque<i64>,
    requested: Option<i64>,
    /// Dernière miniature affichée (montrée en attendant la suivante).
    shown: Option<i64>,
}

impl Thumbs {
    fn poll(&mut self, ctx: &egui::Context) {
        let Some(worker) = &self.worker else { return };
        for (t, image) in worker.poll() {
            let key = millis(t);
            let texture = ctx.load_texture(format!("miniature-{key}"), image, egui::TextureOptions::LINEAR);
            if self.cache.insert(key, texture).is_none() {
                self.order.push_back(key);
            }
            while self.order.len() > THUMB_CACHE {
                if let Some(old) = self.order.pop_front() {
                    self.cache.remove(&old);
                }
            }
        }
    }

    /// Info-bulle de survol : miniature et position. La précision demandée
    /// dépend du zoom de la frise (`span` : durée visible).
    fn ui(&mut self, ui: &mut egui::Ui, path: &Path, t: f64, span: f64, aspect: f32) {
        let step = (span / 300.0).clamp(0.04, 5.0);
        let target = (t / step).round() * step;
        let key = millis(target);
        let ready = self.cache.contains_key(&key);
        if ready {
            self.shown = Some(key);
        } else if self.requested != Some(key) {
            self.worker.get_or_insert_with(|| Thumbnailer::start(path)).request(target);
            self.requested = Some(key);
        }
        let size = egui::vec2(200.0, (200.0 / aspect.max(0.1)).min(200.0));
        match self.shown.and_then(|k| self.cache.get(&k)) {
            Some(texture) => {
                ui.image((texture.id(), size));
            }
            None => {
                ui.add_sized(size, egui::Spinner::new());
            }
        }
        ui.label(egui::RichText::new(format_time_precise(t)).monospace());
        if !ready {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
    }
}

fn millis(t: f64) -> i64 {
    (t * 1000.0).round() as i64
}

#[derive(Default)]
struct App {
    player: Option<Player>,
    error: Option<String>,
    /// Utiliser le décodage matériel quand il est disponible.
    hardware: bool,
    /// Réglages conservés entre les sessions.
    settings: Settings,
    export: ExportState,
    /// Fichier à ouvrir après la fenêtre d'enregistrement.
    pending_open: Option<PathBuf>,
    /// Dernier mouvement de souris ou appui de touche (plein écran).
    last_activity: Option<Instant>,
    /// Souris au-dessus des contrôles flottants du plein écran.
    controls_hovered: bool,
    /// Message en surimpression (volume, déplacement, montage…).
    osd: Option<(String, Instant)>,
    /// Icône lecture / pause affichée brièvement au centre de l'image.
    flash: Option<(&'static str, Instant)>,
    /// Clic sur l'image en attente (lecture / pause, sauf double-clic).
    pending_click: Option<Instant>,
    show_help: bool,
    /// Champs « début » et « fin » du panneau des passages.
    cut_form: [String; 2],
    cut_form_error: Option<String>,
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
    Running {
        job: export::Export,
        started: Instant,
    },
    Finished(Result<PathBuf, String>),
}

impl App {
    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        match Player::new(path.clone(), self.hardware) {
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
                player.decoder.playback.set_volume(self.settings.volume, self.settings.muted);
                self.settings.add_recent(&player.path);
                self.player = Some(player);
            }
            Err(e) => {
                if !path.exists() {
                    self.settings.remove_recent(&path);
                }
                self.error = Some(format!("{e:#}"));
            }
        }
    }

    /// Rouvre le fichier courant (changement de mode de décodage) en
    /// conservant la position, l'état de pause, le volume et le montage.
    fn reopen(&mut self, ctx: &egui::Context) {
        let Some(mut old) = self.player.take() else { return };
        let (position, paused) = (old.position(), old.paused());
        let (volume, muted) = old.decoder.playback.volume();
        let path = old.path.clone();
        let editor = std::mem::take(&mut old.editor);
        drop(old);
        self.open(ctx, path);
        if let Some(player) = &mut self.player {
            player.editor = editor;
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
        let dir = self
            .player
            .as_ref()
            .map(|p| p.path.as_path())
            .or(self.settings.recent.first().map(PathBuf::as_path))
            .and_then(Path::parent);
        if let Some(dir) = dir {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.open(ctx, path);
        }
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.osd = Some((text.into(), Instant::now()));
    }

    /// Lecture / pause. Avec l'aperçu du montage, relancer la lecture à la
    /// fin du dernier passage conservé repart du premier.
    fn toggle_pause(&mut self) {
        let Some(player) = &mut self.player else { return };
        let kept = player.editor.cuts().kept(player.duration());
        match (kept.first(), kept.last()) {
            (Some(first), Some(last))
                if self.settings.preview_cuts && player.paused() && player.position() >= last.1 - 0.05 =>
            {
                player.seek(first.0);
                player.decoder.playback.set_paused(false);
            }
            _ => player.toggle_pause(),
        }
        let icon = if player.paused() { icon::PAUSE } else { icon::PLAY };
        self.flash = Some((icon, Instant::now()));
    }

    fn seek_relative(&mut self, delta: f64) {
        let Some(player) = &mut self.player else { return };
        let target = (player.position() + delta).clamp(0.0, player.duration().max(0.0));
        player.seek(target);
        let sign = if delta < 0.0 { "−" } else { "+" };
        self.notify(format!("{sign}{} s   {}", delta.abs(), format_time(target)));
    }

    fn change_volume(&mut self, delta: f32) {
        let Some(player) = &self.player else { return };
        let playback = &player.decoder.playback;
        let (volume, _) = playback.volume();
        playback.set_volume(volume + delta, false);
        let (volume, _) = playback.volume();
        self.notify(format!("Volume {:.0} %", volume * 100.0));
    }

    fn toggle_mute(&mut self) {
        let Some(player) = &self.player else { return };
        let playback = &player.decoder.playback;
        let (volume, muted) = playback.volume();
        playback.set_volume(volume, !muted);
        self.notify(if muted { "Son rétabli" } else { "Son coupé" });
    }

    /// Pendant la lecture, saute les passages supprimés (aperçu du montage).
    fn skip_removed(&mut self) {
        let Some(player) = &mut self.player else { return };
        if !self.settings.preview_cuts || player.paused() || player.awaiting_frame {
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

        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let pressed = |key| ctx.input(|i| i.key_pressed(key));
        let (command, shift) = ctx.input(|i| (i.modifiers.command, i.modifiers.shift));
        if command && pressed(egui::Key::O) {
            self.open_dialog(ctx);
            return;
        }
        if pressed(egui::Key::F1) || pressed(egui::Key::Questionmark) {
            self.show_help = true;
            return;
        }
        if self.player.is_none() {
            return;
        }

        if pressed(egui::Key::Space) {
            self.toggle_pause();
        }
        if pressed(egui::Key::ArrowLeft) {
            self.seek_relative(-SEEK_STEP);
        }
        if pressed(egui::Key::ArrowRight) {
            self.seek_relative(SEEK_STEP);
        }
        if pressed(egui::Key::ArrowUp) {
            self.change_volume(VOLUME_STEP);
        }
        if pressed(egui::Key::ArrowDown) {
            self.change_volume(-VOLUME_STEP);
        }
        if pressed(egui::Key::M) {
            self.toggle_mute();
        }
        if pressed(egui::Key::F) {
            toggle_fullscreen(ctx);
        }
        if pressed(egui::Key::Escape) && is_fullscreen(ctx) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }

        let Some(player) = &mut self.player else { return };
        let duration = player.duration();
        let position = player.position();
        let mut message = None;
        if pressed(egui::Key::Comma) {
            player.step_frame(-1.0);
        }
        if pressed(egui::Key::Period) {
            player.step_frame(1.0);
        }
        for (key, is_in) in [(egui::Key::I, true), (egui::Key::O, false)] {
            if !pressed(key) {
                continue;
            }
            let mark = if is_in { player.editor.mark_in } else { player.editor.mark_out };
            if shift {
                if let Some(m) = mark {
                    player.seek(m);
                }
                continue;
            }
            if is_in {
                player.editor.mark_in = Some(position);
            } else {
                player.editor.mark_out = Some(position);
            }
            self.settings.show_montage = true;
            let what = if is_in { "Début" } else { "Fin" };
            message = Some(format!("{what} de la sélection : {}", format_time_precise(position)));
        }
        if (pressed(egui::Key::Delete) || pressed(egui::Key::Backspace)) && player.editor.cut_selection(duration) {
            message = Some("Passage supprimé".to_owned());
        }
        if command && pressed(egui::Key::Z) && player.editor.can_undo() {
            player.editor.undo();
            message = Some("Coupe annulée".to_owned());
        }
        if let Some(message) = message {
            self.notify(message);
        }
    }

    /// Contrôles de lecture : frise, boutons, montage. `overlay` : version
    /// flottante du plein écran.
    fn controls(&mut self, ui: &mut egui::Ui, overlay: bool) {
        let ctx = ui.ctx().clone();
        let detailed = self.settings.show_montage;
        let Some(player) = &mut self.player else { return };
        let duration = player.duration();
        let position = player.position();
        let playing = !player.paused() && !player.finished();
        let mut message = None;

        ui.add_space(4.0);
        if duration > 0.0 {
            if detailed {
                player.keyframes();
                if player.waveform.is_none() {
                    player.waveform = Some(Waveform::start(&player.path));
                }
            }
            let Player { editor, thumbs, timeline: state, path, decoder, keyframes, waveform, .. } = &mut *player;
            let keys = match keyframes {
                KeyframeScan::Done(Some(keys)) => Some(keys.as_slice()),
                _ => None,
            };
            let peaks = waveform.as_ref().map(|w| w.peaks());
            let input = timeline::Input {
                duration,
                position,
                playing,
                detailed,
                keyframes: keys,
                peaks: peaks.as_deref(),
            };
            let aspect = decoder.info.display_aspect;
            let out = timeline::show(ui, state, editor, &input, &mut |ui, t, span| {
                thumbs.ui(ui, path, t, span, aspect)
            });
            drop(peaks);
            if let Some(t) = out.seek {
                player.seek(t);
            }
            message = out.message;
        }

        let compact = ui.available_width() < 820.0;
        let (mut toggle, mut open, mut reopen, mut mute, mut reveal) = (false, false, false, false, false);
        let (mut seek_by, mut step) = (None, None);
        ui.horizontal(|ui| {
            if theme::icon_button(ui, icon::FOLDER_OPEN, "Ouvrir une vidéo (Ctrl+O)").clicked() {
                open = true;
            }
            ui.separator();
            if theme::icon_button(ui, icon::REWIND, "Reculer de 5 s (←)").clicked() {
                seek_by = Some(-SEEK_STEP);
            }
            if !compact && theme::icon_button(ui, icon::CARET_LEFT, "Image précédente (,)").clicked() {
                step = Some(-1.0);
            }
            let (play_icon, play_tip) =
                if playing { (icon::PAUSE, "Pause (Espace)") } else { (icon::PLAY, "Lecture (Espace)") };
            if theme::accent_button(ui, play_icon, 20.0, egui::vec2(44.0, 30.0)).on_hover_text(play_tip).clicked() {
                toggle = true;
            }
            if !compact && theme::icon_button(ui, icon::CARET_RIGHT, "Image suivante (.)").clicked() {
                step = Some(1.0);
            }
            if theme::icon_button(ui, icon::FAST_FORWARD, "Avancer de 5 s (→)").clicked() {
                seek_by = Some(SEEK_STEP);
            }
            if !compact && theme::icon_button(ui, icon::STOP, "Stop").clicked() {
                player.stop();
            }
            ui.add_space(4.0);
            // Chiffres à chasse fixe : l'affichage ne bouge pas pendant la lecture.
            ui.label(egui::RichText::new(format_time_precise(position)).monospace().strong());
            ui.label(egui::RichText::new(format!("/ {}", format_time_precise(duration))).monospace().weak());
            if let Some(zoom) = player.timeline.zoom(duration)
                && ui
                    .add(egui::Button::new(
                        egui::RichText::new(format!("{}  ×{zoom:.1}", icon::MAGNIFYING_GLASS_MINUS)).small(),
                    ))
                    .on_hover_text("Zoom de la frise (molette) : cliquer pour tout afficher")
                    .clicked()
            {
                player.timeline.reset_zoom();
            }

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

                ui.menu_button(egui::RichText::new(icon::GEAR).size(18.0), |ui| {
                    ui.set_min_width(230.0);
                    if ui
                        .checkbox(&mut self.hardware, "Accélération matérielle")
                        .on_hover_text("Décodage par le GPU (VA-API, NVDEC, VDPAU…)")
                        .changed()
                    {
                        self.settings.hardware = self.hardware;
                        reopen = true;
                    }
                    ui.separator();
                    ui.weak("Thème");
                    for (choice, symbol, label) in [
                        (ThemeChoice::Dark, icon::MOON, "Sombre"),
                        (ThemeChoice::Light, icon::SUN, "Clair"),
                        (ThemeChoice::System, icon::MONITOR, "Comme le système"),
                    ] {
                        if ui.radio_value(&mut self.settings.theme, choice, format!("{symbol}  {label}")).changed() {
                            theme::apply(&ctx, choice);
                        }
                    }
                    ui.separator();
                    if ui.button(format!("{}  Raccourcis clavier   (F1)", icon::KEYBOARD)).clicked() {
                        self.show_help = true;
                        ui.close();
                    }
                })
                .response
                .on_hover_text("Réglages");

                ui.menu_button(egui::RichText::new(icon::INFO).size(18.0), |ui| {
                    let info = &player.decoder.info;
                    let name = player.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let folder = player.path.parent().map(|d| d.display().to_string()).unwrap_or_default();
                    let mut rows = vec![
                        ("Fichier", name),
                        ("Dossier", folder),
                        ("Codec", info.codec_name.clone()),
                        ("Résolution", format!("{} × {}", info.width, info.height)),
                        ("Conteneur", info.container.clone()),
                        ("Durée", format_time_precise(duration)),
                        ("Décodage", player.decoding_label()),
                        (
                            "Audio",
                            info.audio.clone().or(info.audio_error.clone()).unwrap_or_else(|| "aucune piste".into()),
                        ),
                    ];
                    if info.fps > 0.0 {
                        rows.insert(4, ("Images/s", format!("{:.3}", info.fps)));
                    }
                    ui.set_max_width(440.0);
                    egui::Grid::new("informations").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                        for (label, value) in rows {
                            ui.weak(label);
                            ui.add(egui::Label::new(value).wrap());
                            ui.end_row();
                        }
                    });
                    ui.separator();
                    if ui.button(format!("{}  Afficher dans le dossier", icon::FOLDER)).clicked() {
                        reveal = true;
                        ui.close();
                    }
                })
                .response
                .on_hover_text("Informations sur la vidéo");

                ui.menu_button(egui::RichText::new(icon::AT).size(18.0), about)
                    .response
                    .on_hover_text("À propos de Mnémosyne");

                if duration > 0.0 {
                    let label = if compact {
                        icon::SCISSORS.to_owned()
                    } else {
                        format!("{}  Montage", icon::SCISSORS)
                    };
                    if ui
                        .add(egui::Button::new(label).selected(self.settings.show_montage).min_size(egui::vec2(34.0, 30.0)))
                        .on_hover_text("Afficher / masquer les outils de montage")
                        .clicked()
                    {
                        self.settings.show_montage = !self.settings.show_montage;
                    }
                }
                if player.decoder.has_audio() {
                    ui.separator();
                    let (mut volume, muted) = player.decoder.playback.volume();
                    ui.spacing_mut().slider_width = if compact { 60.0 } else { 90.0 };
                    if ui
                        .add(
                            egui::Slider::new(&mut volume, 0.0..=1.0)
                                .show_value(false)
                                .handle_shape(egui::style::HandleShape::Circle),
                        )
                        .changed()
                    {
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
                        mute = true;
                    }
                }
            });
        });
        let path = player.path.clone();
        if toggle {
            self.toggle_pause();
        }
        if let Some(delta) = seek_by {
            self.seek_relative(delta);
        }
        if let Some(direction) = step
            && let Some(player) = &mut self.player
        {
            player.step_frame(direction);
        }
        if mute {
            self.toggle_mute();
        }
        if reveal && let Err(e) = reveal_in_folder(&path) {
            self.error = Some(format!("Impossible d'ouvrir le dossier : {e}"));
        }
        if let Some(message) = message {
            self.notify(message);
        }
        if open {
            self.open_dialog(&ctx);
        }

        if self.settings.show_montage && duration > 0.0 {
            ui.add_space(2.0);
            ui.separator();
            self.montage_controls(ui, overlay);
        }
        if reopen {
            self.reopen(&ctx);
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().warn_fg_color, error);
        }
        ui.add_space(4.0);
    }

    /// Barre d'outils de montage.
    fn montage_controls(&mut self, ui: &mut egui::Ui, overlay: bool) {
        let Some(player) = &mut self.player else { return };
        let duration = player.duration();
        if duration <= 0.0 {
            return;
        }
        let position = player.position();
        let editor = &mut player.editor;
        let (mut open_export, mut message, mut goto) = (false, None, None);

        ui.horizontal_wrapped(|ui| {
            if ui
                .button(format!("{}  Début [I]", icon::ARROW_LINE_RIGHT))
                .on_hover_text("Début de la sélection à la position actuelle (touche I)")
                .clicked()
            {
                editor.mark_in = Some(position);
            }
            if ui
                .button(format!("{}  Fin [O]", icon::ARROW_LINE_LEFT))
                .on_hover_text("Fin de la sélection à la position actuelle (touche O)")
                .clicked()
            {
                editor.mark_out = Some(position);
            }
            let (mark_in, mark_out) = (editor.mark_in, editor.mark_out);
            if ui
                .add_enabled(mark_in.is_some(), egui::Button::new(icon::SKIP_BACK))
                .on_hover_text("Aller au début de la sélection (Maj+I)")
                .clicked()
            {
                goto = mark_in;
            }
            if ui
                .add_enabled(mark_out.is_some(), egui::Button::new(icon::SKIP_FORWARD))
                .on_hover_text("Aller à la fin de la sélection (Maj+O)")
                .clicked()
            {
                goto = mark_out;
            }
            let selection = editor.selection();
            if ui
                .add_enabled(selection.is_some(), egui::Button::new(format!("{}  Supprimer la sélection", icon::TRASH)))
                .on_hover_text("Supprime le passage entre le début et la fin de la sélection (touche Suppr)")
                .clicked()
                && editor.cut_selection(duration)
            {
                message = Some("Passage supprimé");
            }
            ui.separator();
            if ui.button("Couper avant").on_hover_text("Supprime tout ce qui précède la position actuelle").clicked() {
                editor.cut_before(position, duration);
                message = Some("Début supprimé");
            }
            if ui.button("Couper après").on_hover_text("Supprime tout ce qui suit la position actuelle").clicked() {
                editor.cut_after(position, duration);
                message = Some("Fin supprimée");
            }
            ui.separator();
            if ui
                .add_enabled(editor.can_undo(), egui::Button::new(format!("{}  Annuler", icon::ARROW_U_UP_LEFT)))
                .on_hover_text("Ctrl+Z")
                .clicked()
            {
                editor.undo();
                message = Some("Coupe annulée");
            }
            if ui
                .add_enabled(
                    !editor.cuts().is_empty() || selection.is_some(),
                    egui::Button::new(format!("{}  Tout rétablir", icon::ARROWS_CLOCKWISE)),
                )
                .on_hover_text("Rétablit toute la vidéo et efface la sélection")
                .clicked()
            {
                editor.reset();
                message = Some("Vidéo entière rétablie");
            }
            ui.checkbox(&mut self.settings.preview_cuts, "Aperçu")
                .on_hover_text("Saute les passages supprimés pendant la lecture");
            if !overlay
                && ui
                    .add(
                        egui::Button::new(format!("{}  Passages", icon::LIST_BULLETS))
                            .selected(self.settings.show_segments),
                    )
                    .on_hover_text("Liste des passages et coupe à des temps précis")
                    .clicked()
            {
                self.settings.show_segments = !self.settings.show_segments;
            }
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
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!(
                            "Sélection : {} → {} ({:.1} s)",
                            format_time_precise(a),
                            format_time_precise(b),
                            b - a
                        ))
                        .weak(),
                    )
                    .wrap_mode(egui::TextWrapMode::Extend),
                );
            } else if let Some(m) = editor.mark_in.or(editor.mark_out) {
                ui.weak(format!("Repère à {}", format_time_precise(m)));
            }
            // Légende des couleurs de la frise.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                legend(ui, theme::REMOVED, "supprimé", false);
                ui.add_space(6.0);
                legend(ui, theme::SELECTION, "sélection", false);
                ui.add_space(6.0);
                legend(ui, ui.visuals().weak_text_color(), "image clé", true);
            });
        });

        if let Some(t) = goto {
            player.seek(t);
        }
        if open_export {
            player.decoder.playback.set_paused(true);
            self.export = ExportState::Setup {
                destination: default_destination(&player.path),
                mode: if self.settings.fast_export { export::Mode::Fast } else { export::Mode::Precise },
                overwrite: false,
                error: None,
            };
        }
        if let Some(message) = message {
            self.notify(message);
        }
    }

    /// Panneau latéral : liste des passages conservés et supprimés, coupe à
    /// des temps saisis au clavier.
    fn segments_panel(&mut self, ui: &mut egui::Ui) {
        let Some(player) = &mut self.player else { return };
        let duration = player.duration();
        let position = player.position();
        let (mut goto, mut message) = (None, None);

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(icon::LIST_BULLETS).size(18.0).color(ACCENT));
            ui.strong("Passages");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(egui::Button::new(icon::X).frame(false)).on_hover_text("Fermer le panneau").clicked() {
                    self.settings.show_segments = false;
                }
            });
        });
        ui.add_space(4.0);

        let editor = &mut player.editor;
        let mut rows: Vec<(f64, f64, bool)> =
            editor.cuts().kept(duration).into_iter().map(|(s, e)| (s, e, true)).collect();
        rows.extend(editor.cuts().removed().iter().map(|&(s, e)| (s, e, false)));
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));

        let form_height = 170.0;
        egui::ScrollArea::vertical()
            .max_height((ui.available_height() - form_height).max(80.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (s, e, kept) in rows {
                    let current = position >= s && position < e;
                    let color = if kept { ACCENT } else { theme::REMOVED };
                    let fill = if current { color.gamma_multiply(0.12) } else { ui.visuals().faint_bg_color };
                    egui::Frame::NONE.fill(fill).corner_radius(6).inner_margin(egui::Margin::symmetric(8, 6)).show(
                        ui,
                        |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let (bar, _) = ui.allocate_exact_size(egui::vec2(4.0, 32.0), egui::Sense::hover());
                                ui.painter().rect_filled(bar, 2.0, color);
                                ui.vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} → {}",
                                            format_time_precise(s),
                                            format_time_precise(e)
                                        ))
                                        .monospace(),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{:.1} s · {}",
                                            e - s,
                                            if kept { "conservé" } else { "supprimé" }
                                        ))
                                        .small()
                                        .weak(),
                                    );
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if kept {
                                        if ui.small_button(icon::TRASH).on_hover_text("Supprimer ce passage").clicked() {
                                            editor.cut_range(s, e, duration);
                                            message = Some("Passage supprimé");
                                        }
                                    } else if ui
                                        .small_button(icon::ARROWS_CLOCKWISE)
                                        .on_hover_text("Rétablir ce passage")
                                        .clicked()
                                    {
                                        editor.restore(s, e);
                                        message = Some("Passage rétabli");
                                    }
                                    if ui.small_button(icon::CROSSHAIR).on_hover_text("Aller au début du passage").clicked() {
                                        goto = Some(s);
                                    }
                                });
                            });
                        },
                    );
                    ui.add_space(2.0);
                }
            });

        ui.separator();
        ui.strong("Couper à des temps précis");
        ui.label(egui::RichText::new("Formats : 83.4 · 1:23.4 · 1:02:03").small().weak());
        for (index, label) in ["Début", "Fin"].into_iter().enumerate() {
            ui.horizontal(|ui| {
                // Champs alignés quelle que soit la largeur du libellé.
                let width = ui.label(label).rect.width();
                ui.add_space((44.0 - width).max(0.0));
                ui.add_sized(
                    [110.0, 24.0],
                    egui::TextEdit::singleline(&mut self.cut_form[index]).hint_text("mm:ss.d"),
                );
                if ui.small_button(icon::CROSSHAIR).on_hover_text("Position actuelle").clicked() {
                    self.cut_form[index] = format_time_precise(position);
                }
            });
        }
        let parsed = (parse_time(&self.cut_form[0]), parse_time(&self.cut_form[1]));
        ui.horizontal(|ui| {
            let select =
                ui.button(format!("{}  Sélectionner", icon::CHECK)).on_hover_text("Place les repères de sélection");
            let cut = ui.button(format!("{}  Supprimer", icon::TRASH)).on_hover_text("Supprime ce passage");
            if select.clicked() || cut.clicked() {
                match parsed {
                    (Some(a), Some(b)) if a.max(b) <= duration + 0.05 && (b - a).abs() > 1e-3 => {
                        self.cut_form_error = None;
                        if cut.clicked() {
                            editor.cut_range(a, b, duration);
                            message = Some("Passage supprimé");
                        } else {
                            editor.mark_in = Some(a.min(b));
                            editor.mark_out = Some(a.max(b));
                            goto = Some(a.min(b));
                        }
                    }
                    (Some(_), Some(_)) => {
                        self.cut_form_error =
                            Some(format!("Temps hors de la vidéo (durée {}).", format_time_precise(duration)))
                    }
                    _ => self.cut_form_error = Some("Temps non reconnu.".to_owned()),
                }
            }
        });
        if let Some(error) = &self.cut_form_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }

        if let Some(t) = goto {
            player.seek(t);
        }
        if let Some(message) = message {
            self.notify(message);
        }
    }

    /// Écran d'accueil, sans vidéo ouverte.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let area = ui.max_rect();
        let recent: Vec<PathBuf> = self.settings.recent.iter().take(RECENT_SHOWN).cloned().collect();
        let extra = if recent.is_empty() { 0.0 } else { 34.0 + 30.0 * recent.len() as f32 };
        let zone = egui::Rect::from_center_size(
            area.center(),
            egui::vec2((area.width() - 48.0).clamp(200.0, 560.0), (area.height() - 48.0).clamp(200.0, 340.0 + extra)),
        );
        let hovering = files_hovered(&ctx);
        let color = if hovering { ACCENT } else { ui.visuals().widgets.noninteractive.bg_stroke.color };
        if hovering {
            ui.painter().rect_filled(zone, 16.0, ACCENT.gamma_multiply(0.08));
        }
        dashed_rect(ui.painter(), zone, color);

        let (mut open, mut open_path) = (false, None);
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(zone.shrink(16.0)).layout(egui::Layout::top_down(egui::Align::Center)),
            |ui| {
                let content = 240.0 + extra + if self.error.is_some() { 30.0 } else { 0.0 };
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
                .on_hover_text("Ctrl+O")
                .clicked()
                {
                    open = true;
                }
                ui.add_space(6.0);
                if hovering {
                    ui.label(
                        egui::RichText::new(format!("{}  Déposez le fichier pour l'ouvrir", icon::UPLOAD)).color(ACCENT),
                    );
                } else {
                    ui.weak("ou glissez un fichier dans cette fenêtre");
                }
                ui.label(egui::RichText::new("MP4  ·  MKV  ·  WebM  ·  AVI").small().weak());
                if let Some(error) = &self.error {
                    ui.add_space(10.0);
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                if !recent.is_empty() {
                    ui.add_space(18.0);
                    ui.label(
                        egui::RichText::new(format!("{}  Récemment ouverts", icon::CLOCK_COUNTER_CLOCKWISE))
                            .small()
                            .weak(),
                    );
                    ui.add_space(2.0);
                    for path in recent {
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let exists = path.exists();
                        let mut text = egui::RichText::new(format!("{}  {name}", icon::FILE_VIDEO));
                        if !exists {
                            text = text.weak().strikethrough();
                        }
                        let tip =
                            if exists { path.display().to_string() } else { format!("{} (introuvable)", path.display()) };
                        if ui.add(egui::Button::new(text).frame(false)).on_hover_text(tip).clicked() {
                            open_path = Some(path);
                        }
                    }
                }
            },
        );
        if open {
            self.open_dialog(&ctx);
        }
        if let Some(path) = open_path {
            self.open(&ctx, path);
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
        let mut reveal = None;

        let response = egui::Modal::new(egui::Id::unique("enregistrement")).show(ctx, |ui| {
            ui.set_width(480.0);
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
                    ui.add_space(8.0);
                    ui.columns(2, |columns| {
                        mode_card(
                            &mut columns[0],
                            mode,
                            export::Mode::Precise,
                            icon::TARGET,
                            "Précis",
                            "Coupes à l'image près. Réencodage : plus lent, qualité quasi identique.",
                        );
                        mode_card(
                            &mut columns[1],
                            mode,
                            export::Mode::Fast,
                            icon::LIGHTNING,
                            "Rapide",
                            "Sans perte, quasi instantané. Coupes calées sur les images clés.",
                        );
                    });
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
                        if theme::accent_button(
                            ui,
                            format!("{}  Enregistrer", icon::FLOPPY_DISK),
                            14.0,
                            egui::vec2(0.0, 28.0),
                        )
                        .clicked()
                        {
                            if export::same_file(&source, destination) {
                                *error = Some("C'est le fichier d'origine : choisissez un autre nom.".into());
                            } else if destination.exists() && !*overwrite {
                                *error = Some("Ce fichier existe déjà : choisissez un autre nom.".into());
                            } else {
                                self.settings.fast_export = *mode == export::Mode::Fast;
                                match export::Export::start(&source, destination, segments.clone(), *mode) {
                                    Ok(job) => next = Some(ExportState::Running { job, started: Instant::now() }),
                                    Err(e) => *error = Some(format!("{e:#}")),
                                }
                            }
                        }
                        if ui.button("Annuler").clicked() {
                            next = Some(ExportState::Closed);
                        }
                    });
                }
                ExportState::Running { job, started } => {
                    ui.label(format!("Écriture de {}", job.destination.display()));
                    let progress = job.progress();
                    ui.add(egui::ProgressBar::new(progress).show_percentage());
                    let elapsed = started.elapsed().as_secs_f64();
                    if progress > 0.02 && elapsed > 1.0 {
                        let remaining = elapsed * f64::from(1.0 - progress) / f64::from(progress);
                        ui.weak(format!("Temps restant : environ {}", format_duration(remaining)));
                    } else {
                        ui.weak("Estimation du temps restant…");
                    }
                    if ui.button("Annuler").clicked() {
                        job.cancel();
                    }
                    if let Some(result) = job.poll() {
                        next = Some(ExportState::Finished(result.map(|()| job.destination.clone())));
                        // Signale la fin si la fenêtre n'est pas au premier plan.
                        if !ctx.input(|i| i.viewport().focused.unwrap_or(true)) {
                            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                                egui::UserAttentionType::Informational,
                            ));
                        }
                    }
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                ExportState::Finished(result) => {
                    match result {
                        Ok(path) => {
                            ui.label(format!("{}  Vidéo enregistrée :", icon::CHECK));
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
                        if let Ok(path) = result {
                            if theme::accent_button(ui, "Ouvrir la vidéo enregistrée", 14.0, egui::vec2(0.0, 28.0))
                                .clicked()
                            {
                                self.pending_open = Some(path.clone());
                            }
                            if ui.button(format!("{}  Afficher dans le dossier", icon::FOLDER)).clicked() {
                                reveal = Some(path.clone());
                            }
                        }
                        if ui.button("Fermer").clicked() {
                            next = Some(ExportState::Closed);
                        }
                    });
                }
            }
        });

        // Échap / clic à côté : ferme, sauf pendant l'écriture.
        if response.should_close() && !matches!(self.export, ExportState::Running { .. }) {
            next = Some(ExportState::Closed);
        }
        if let Some(state) = next {
            self.export = state;
        }
        if let Some(path) = reveal
            && let Err(e) = reveal_in_folder(&path)
        {
            self.error = Some(format!("Impossible d'ouvrir le dossier : {e}"));
        }
        if let Some(path) = self.pending_open.take() {
            self.export = ExportState::Closed;
            self.open(ctx, path);
        }
    }

    /// Fenêtre d'aide : raccourcis clavier et souris.
    fn help_window(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let response = egui::Modal::new(egui::Id::unique("aide")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon::KEYBOARD).size(22.0).color(ACCENT));
                ui.heading("Raccourcis");
            });
            ui.add_space(6.0);
            egui::Grid::new("raccourcis").num_columns(2).striped(true).spacing([24.0, 6.0]).show(ui, |ui| {
                for (keys, action) in SHORTCUTS {
                    ui.label(egui::RichText::new(*keys).strong());
                    ui.label(*action);
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                if ui.button("Fermer").clicked() {
                    close = true;
                }
            });
        });
        if close || response.should_close() {
            self.show_help = false;
        }
    }

    /// Surimpressions sur l'image : icône lecture / pause, message, dépôt.
    fn overlays(&mut self, ui: &egui::Ui, area: egui::Rect) {
        let ctx = ui.ctx();
        let painter = ui.painter();
        if let Some((symbol, since)) = self.flash {
            let t = since.elapsed().as_secs_f32() / FLASH_DURATION.as_secs_f32();
            if t < 1.0 {
                let alpha = 1.0 - t;
                let radius = 40.0 * (1.0 + 0.25 * t);
                painter.circle_filled(area.center(), radius, egui::Color32::from_black_alpha((150.0 * alpha) as u8));
                painter.text(
                    area.center(),
                    egui::Align2::CENTER_CENTER,
                    symbol,
                    egui::FontId::proportional(36.0 * (1.0 + 0.25 * t)),
                    egui::Color32::WHITE.gamma_multiply(alpha),
                );
                ctx.request_repaint();
            } else {
                self.flash = None;
            }
        }
        if let Some((text, since)) = &self.osd {
            let elapsed = since.elapsed();
            if elapsed < OSD_DURATION {
                // Fondu sur les 300 dernières millisecondes.
                let left = (OSD_DURATION - elapsed).as_secs_f32();
                let alpha = (left / 0.3).min(1.0);
                let galley = painter.layout_no_wrap(
                    text.clone(),
                    egui::FontId::proportional(16.0),
                    egui::Color32::WHITE.gamma_multiply(alpha),
                );
                let pos = area.center_top() + egui::vec2(-galley.size().x / 2.0, 24.0);
                let frame = egui::Rect::from_min_size(pos, galley.size()).expand2(egui::vec2(14.0, 8.0));
                painter.rect_filled(frame, 8.0, egui::Color32::from_black_alpha((170.0 * alpha) as u8));
                painter.galley(pos, galley, egui::Color32::WHITE);
                ctx.request_repaint();
            } else {
                self.osd = None;
            }
        }
        if files_hovered(ctx) {
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
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let export_open = !matches!(self.export, ExportState::Closed);
        let modal_open = export_open || self.show_help;
        if !modal_open {
            self.handle_input(&ctx);
        }

        if let Some(player) = &mut self.player
            && let Some(e) = player.update(&ctx)
        {
            self.error = Some(e);
        }
        self.skip_removed();
        if export_open {
            self.export_window(&ctx);
        }
        if self.show_help {
            self.help_window(&ctx);
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

        let has_video = self.player.is_some();
        let duration = self.player.as_ref().map_or(0.0, Player::duration);
        if has_video && !fullscreen {
            egui::Panel::bottom("controles")
                .frame(egui::Frame::side_top_panel(&ctx.global_style()).inner_margin(egui::Margin::symmetric(12, 4)))
                .show(ui, |ui| self.controls(ui, false));
            if self.settings.show_montage && self.settings.show_segments && duration > 0.0 {
                egui::Panel::right("passages")
                    .default_size(280.0)
                    .size_range(230.0..=420.0)
                    .frame(
                        egui::Frame::side_top_panel(&ctx.global_style()).inner_margin(egui::Margin::symmetric(10, 4)),
                    )
                    .show(ui, |ui| self.segments_panel(ui));
            }
        }

        let background = if has_video { egui::Color32::BLACK } else { ctx.global_style().visuals.extreme_bg_color };
        let (mut clicked, mut double_clicked) = (false, false);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(background))
            .show(ui, |ui| {
                let area = ui.max_rect();
                if !has_video {
                    self.welcome(ui);
                    self.overlays(ui, area);
                    return;
                }
                let response = ui.interact(area, egui::Id::unique("video"), egui::Sense::click());
                double_clicked = response.double_clicked();
                clicked = response.clicked();
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
                self.overlays(ui, area);
            });

        // Clic sur l'image : lecture / pause, sauf s'il s'agit d'un double-clic.
        if double_clicked {
            self.pending_click = None;
            toggle_fullscreen(&ctx);
        } else if clicked {
            self.pending_click = Some(now);
        }
        if let Some(since) = self.pending_click {
            if since.elapsed() >= CLICK_DELAY {
                self.pending_click = None;
                self.toggle_pause();
            } else {
                ctx.request_repaint_after(CLICK_DELAY.saturating_sub(since.elapsed()));
            }
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

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Some(player) = &self.player {
            (self.settings.volume, self.settings.muted) = player.decoder.playback.volume();
        }
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }
}

/// Contenu du menu « À propos ».
fn about(ui: &mut egui::Ui) {
    ui.set_max_width(340.0);
    // Les menus justifient le texte ; ici, il est aligné à gauche.
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), about_content);
}

fn about_content(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading("Mnémosyne");
        ui.weak(concat!("version ", env!("CARGO_PKG_VERSION")));
    });
    ui.add(
        egui::Label::new(
            "Lecteur vidéo minimaliste pour le H.264 et le H.265, avec décodage \
             matériel et montage simple\u{a0}: supprimez des passages, puis enregistrez \
             la vidéo montée en mode rapide ou précis. Basé sur FFmpeg et egui.",
        )
        .wrap(),
    );
    ui.separator();
    ui.weak("Licence");
    ui.add(
        egui::Label::new(
            "Logiciel libre distribué sous licence GNU GPL, version 3 ou ultérieure, \
             sans aucune garantie.",
        )
        .wrap(),
    );
    ui.hyperlink_to("GNU General Public License", "https://www.gnu.org/licenses/gpl-3.0.html");
    ui.separator();
    ui.weak("Auteur");
    ui.label(format!("{}  PoongalOO", icon::USER));
    ui.hyperlink_to(format!("{}  sam@poongaloo.org", icon::ENVELOPE), "mailto:sam@poongaloo.org");
}

/// Choix du mode d'enregistrement, présenté comme une carte cliquable.
fn mode_card(
    ui: &mut egui::Ui,
    current: &mut export::Mode,
    value: export::Mode,
    symbol: &str,
    title: &str,
    text: &str,
) {
    let selected = *current == value;
    let visuals = ui.visuals();
    let stroke = if selected {
        egui::Stroke::new(2.0, ACCENT)
    } else {
        egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color)
    };
    let fill = if selected { ACCENT.gamma_multiply(0.12) } else { visuals.faint_bg_color };
    let weak = visuals.weak_text_color();
    let response = egui::Frame::NONE
        .fill(fill)
        .stroke(stroke)
        .corner_radius(8)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(78.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(symbol).size(20.0).color(if selected { ACCENT } else { weak }));
                ui.strong(title);
            });
            ui.label(egui::RichText::new(text).small().weak());
        })
        .response;
    let response = ui.interact(response.rect, ui.scope_id().with(title), egui::Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if response.clicked() {
        *current = value;
    }
}

/// Pastille de couleur et libellé (légende de la frise).
fn legend(ui: &mut egui::Ui, color: egui::Color32, label: &str, thin: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(if thin { 2.0 } else { 10.0 }, 10.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, if thin { 0.0 } else { 2.0 }, color);
    ui.label(egui::RichText::new(label).small().weak());
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

/// Ouvre le gestionnaire de fichiers sur le dossier contenant `path`.
fn reveal_in_folder(path: &Path) -> std::io::Result<()> {
    use std::process::Command;
    #[cfg(target_os = "windows")]
    let mut child = Command::new("explorer").arg(format!("/select,{}", path.display())).spawn()?;
    #[cfg(target_os = "macos")]
    let mut child = Command::new("open").arg("-R").arg(path).spawn()?;
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut child = Command::new("xdg-open").arg(path.parent().unwrap_or(Path::new("."))).spawn()?;
    // Attend la fin du processus pour ne pas laisser de zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
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

/// Lit un temps saisi : « 83.4 », « 1:23.4 » ou « 1:02:03 » (virgule
/// décimale acceptée).
fn parse_time(text: &str) -> Option<f64> {
    let text = text.trim().replace(',', ".");
    if text.is_empty() {
        return None;
    }
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let (last, rest) = parts.split_last()?;
    let seconds: f64 = last.parse().ok().filter(|s: &f64| s.is_finite() && *s >= 0.0)?;
    if !rest.is_empty() && seconds >= 60.0 {
        return None;
    }
    let mut total = 0.0;
    for (i, part) in rest.iter().enumerate() {
        let value: u64 = part.parse().ok()?;
        if i > 0 && value >= 60 {
            return None;
        }
        total = total * 60.0 + value as f64;
    }
    Some(total * 60.0 + seconds)
}

/// Durée lisible : « 45 s », « 3 min 05 s ».
fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    if total < 60 { format!("{total} s") } else { format!("{} min {:02} s", total / 60, total % 60) }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lecture_des_temps() {
        assert_eq!(parse_time("83.4"), Some(83.4));
        assert_eq!(parse_time(" 1:23,5 "), Some(83.5));
        assert_eq!(parse_time("01:02:03"), Some(3723.0));
        assert_eq!(parse_time(&format_time_precise(754.3)), Some(754.3));
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("1:75"), None);
        assert_eq!(parse_time("1:2:3:4"), None);
        assert_eq!(parse_time("abc"), None);
        assert_eq!(parse_time("-3"), None);
    }

    #[test]
    fn durees_lisibles() {
        assert_eq!(format_duration(44.6), "45 s");
        assert_eq!(format_duration(185.0), "3 min 05 s");
    }
}
