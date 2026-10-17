//! Décodage avec FFmpeg, exécuté dans un thread dédié.
//!
//! Le thread lit les paquets du conteneur (MP4, WebM, AVI, MKV…), décode le
//! flux vidéo principal (sur le GPU si possible) et le flux audio principal.
//! Les images sont converties en RGBA et l'audio rééchantillonné au format de
//! la carte son, puis déposés dans l'état partagé [`Playback`]. L'interface
//! pilote le thread par des commandes (déplacement). Chaque déplacement
//! incrémente une « génération » qui permet d'écarter les données périmées.

use std::path::Path;
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, Context as _, Result};
use eframe::egui;
use ffmpeg_next as ffmpeg;
use ffmpeg::{
    codec, color, ffi, format, frame, media,
    software::{resampling, scaling},
    util::{channel_layout::ChannelLayout, format::Pixel, format::Sample},
};

use crate::audio::AudioOutput;
use crate::hwaccel::{self, HwConfig};
use crate::playback::{Playback, Room};

/// Mémoire maximale occupée par les images RGBA en attente.
const FRAME_QUEUE_BYTES: usize = 256 << 20;
/// Audio décodé d'avance (en secondes).
const AUDIO_QUEUE_SECONDS: f64 = 2.0;

/// Métadonnées du fichier ouvert.
#[derive(Clone, Debug)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    /// Rapport largeur/hauteur d'affichage (tient compte des pixels non carrés).
    pub display_aspect: f32,
    pub codec_name: String,
    pub container: String,
    /// Durée en secondes (0 si inconnue).
    pub duration: f64,
    /// Images par seconde (0 si inconnu).
    pub fps: f64,
    /// Vrai si le codec est H.264/AVC ou H.265/HEVC.
    pub supported_codec: bool,
    /// API de décodage matériel configurée (« vaapi », « cuda »…).
    pub hwaccel: Option<&'static str>,
    /// Description de la piste audio lue.
    pub audio: Option<String>,
    /// Raison pour laquelle le son est indisponible.
    pub audio_error: Option<String>,
}

/// Image décodée, prête à l'affichage.
pub struct VideoFrame {
    pub generation: u64,
    /// Position de l'image en secondes depuis le début de la vidéo.
    pub pts: f64,
    pub image: egui::ColorImage,
    /// Image décodée par le GPU.
    pub hardware: bool,
}

enum Command {
    Seek { target: f64, generation: u64 },
}

/// Poignée côté interface sur le décodage et la sortie audio.
///
/// Lorsqu'elle est détruite, le son s'arrête et le thread se termine.
pub struct Decoder {
    pub info: VideoInfo,
    pub playback: Arc<Playback>,
    commands: Sender<Command>,
    errors: Receiver<String>,
    _audio: Option<AudioOutput>,
}

impl Decoder {
    pub fn open(path: &Path, hardware: bool) -> Result<Self> {
        ffmpeg::init().context("initialisation de FFmpeg")?;
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Error);

        let input = format::input(&path)
            .with_context(|| format!("impossible d'ouvrir « {} »", path.display()))?;

        // Origine commune des horodatages audio et vidéo.
        let start_time = unsafe {
            let t = (*input.as_ptr()).start_time;
            if t != ffi::AV_NOPTS_VALUE { t as f64 / f64::from(ffi::AV_TIME_BASE) } else { 0.0 }
        };

        // ---- Vidéo ----
        let stream = input
            .streams()
            .best(media::Type::Video)
            .ok_or_else(|| anyhow!("aucun flux vidéo dans ce fichier"))?;
        let video_index = stream.index();
        let video_time_base = f64::from(stream.time_base());

        let mut context = codec::context::Context::from_parameters(stream.parameters())?;
        let hw = if hardware { hwaccel::setup(&mut context) } else { None };
        if hw.is_none() {
            // Décodage logiciel multi-thread (indispensable pour la HEVC en haute résolution).
            context.set_threading(codec::threading::Config {
                kind: codec::threading::Type::Frame,
                count: 0, // 0 = automatique
            });
        }
        let video_decoder = context
            .decoder()
            .video()
            .context("aucun décodeur disponible pour ce codec")?;

        let codec_id = video_decoder.id();
        let codec_name = match codec_id {
            codec::Id::H264 => "H.264 / AVC".to_owned(),
            codec::Id::HEVC => "H.265 / HEVC".to_owned(),
            _ => video_decoder
                .codec()
                .map(|c| c.description().to_owned())
                .unwrap_or_else(|| format!("{codec_id:?}")),
        };
        let sar = video_decoder.aspect_ratio();
        let sar = if sar.numerator() > 0 && sar.denominator() > 0 { f64::from(sar) as f32 } else { 1.0 };
        let fps = {
            let r = stream.avg_frame_rate();
            if r.numerator() > 0 && r.denominator() > 0 { f64::from(r) } else { 0.0 }
        };
        let duration = if input.duration() > 0 {
            input.duration() as f64 / f64::from(ffi::AV_TIME_BASE)
        } else if stream.duration() > 0 {
            stream.duration() as f64 * video_time_base
        } else {
            0.0
        };
        let (width, height) = (video_decoder.width(), video_decoder.height());
        let frame_bytes = (width as usize * height as usize * 4).max(1);

        // ---- Audio ----
        let mut audio_desc = None;
        let mut audio_error = None;
        let mut audio_parts = None;
        if let Some(stream) = input.streams().best(media::Type::Audio) {
            let opened = codec::context::Context::from_parameters(stream.parameters())
                .and_then(|c| c.decoder().audio());
            match opened {
                Ok(decoder) => {
                    let name = decoder
                        .codec()
                        .map(|c| c.description().to_owned())
                        .unwrap_or_else(|| format!("{:?}", decoder.id()));
                    audio_desc = Some(format!(
                        "{name}, {} Hz, {} canaux",
                        decoder.rate(),
                        decoder.channels()
                    ));
                    audio_parts = Some((stream.index(), f64::from(stream.time_base()), decoder));
                }
                Err(e) => audio_error = Some(format!("décodeur audio indisponible : {e}")),
            }
        }

        let playback = Playback::new(audio_parts.is_some());
        let mut audio_output = None;
        let mut audio_track = None;
        if let Some((index, time_base, decoder)) = audio_parts {
            match AudioOutput::new(playback.clone()) {
                Ok(output) => {
                    audio_track = Some(AudioTrack {
                        index,
                        time_base,
                        decoder,
                        out_rate: output.sample_rate,
                        out_channels: output.channels,
                        next_pts: 0.0,
                    });
                    audio_output = Some(output);
                }
                Err(e) => {
                    playback.disable_audio_clock();
                    audio_desc = None;
                    audio_error = Some(format!("son désactivé : {e:#}"));
                }
            }
        }

        let info = VideoInfo {
            width,
            height,
            display_aspect: width as f32 * sar / height.max(1) as f32,
            codec_name,
            container: input.format().description().to_owned(),
            duration,
            fps,
            supported_codec: matches!(codec_id, codec::Id::H264 | codec::Id::HEVC),
            hwaccel: hw.map(|h| h.name),
            audio: audio_desc,
            audio_error,
        };

        let (command_tx, command_rx) = mpsc::channel();
        let (error_tx, error_rx) = mpsc::channel();
        let max_audio = audio_track.as_ref().map_or(0, |a| {
            (AUDIO_QUEUE_SECONDS * f64::from(a.out_rate)) as usize * usize::from(a.out_channels)
        });

        // Pool de conversion : rapatriement GPU et conversion RGBA en parallèle.
        let converters = thread::available_parallelism().map_or(2, |n| n.get() / 3).clamp(1, 3);
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(converters);
        let job_rx = Arc::new(Mutex::new(job_rx));
        for i in 0..converters {
            let (jobs, playback, errors) = (job_rx.clone(), playback.clone(), error_tx.clone());
            thread::Builder::new()
                .name(format!("conversion-{i}"))
                .spawn(move || convert_worker(jobs, hw, playback, errors))?;
        }

        let worker = Worker {
            input,
            start_time,
            video: VideoTrack {
                index: video_index,
                time_base: video_time_base,
                decoder: video_decoder,
                frame_duration: if fps > 0.0 { 1.0 / fps } else { 1.0 / 25.0 },
                next_pts: 0.0,
                done: false,
            },
            audio: audio_track,
            playback: playback.clone(),
            commands: command_rx,
            errors: error_tx,
            jobs: job_tx,
            pending: None,
            max_frames: (FRAME_QUEUE_BYTES / frame_bytes).clamp(3, 16),
            max_audio,
        };
        thread::Builder::new()
            .name("decodeur".into())
            .spawn(move || worker.run())?;

        Ok(Self { info, playback, commands: command_tx, errors: error_rx, _audio: audio_output })
    }

    /// Demande un déplacement à `target` secondes, pour la génération donnée
    /// (déjà enregistrée dans l'état partagé par l'appelant).
    pub fn seek(&self, target: f64, generation: u64) {
        let _ = self.commands.send(Command::Seek { target, generation });
    }

    /// Dernière erreur signalée par le thread de décodage.
    pub fn try_error(&self) -> Option<String> {
        self.errors.try_iter().last()
    }

    pub fn has_audio(&self) -> bool {
        self._audio.is_some()
    }
}

struct VideoTrack {
    index: usize,
    time_base: f64,
    decoder: codec::decoder::Video,
    frame_duration: f64,
    next_pts: f64,
    /// Toutes les images de la génération courante ont été produites.
    done: bool,
}

struct AudioTrack {
    index: usize,
    time_base: f64,
    decoder: codec::decoder::Audio,
    out_rate: u32,
    out_channels: u16,
    next_pts: f64,
}

/// Convertisseur vers RGBA, recréé si le format des images change.
struct Scaler {
    context: scaling::Context,
    /// Format et taille source, taille de sortie.
    key: (Pixel, u32, u32, u32, u32),
    /// Tampon de sortie réutilisé (aligné par FFmpeg pour le SIMD).
    output: frame::Video,
}

/// Image décodée à convertir (éventuellement encore en mémoire GPU).
struct Job {
    generation: u64,
    pts: f64,
    frame: frame::Video,
}

/// Rééchantillonneur vers le format de la carte son.
struct Resampler {
    context: resampling::Context,
    key: (Sample, u32, u16),
}

struct Worker {
    input: format::context::Input,
    start_time: f64,
    video: VideoTrack,
    audio: Option<AudioTrack>,
    playback: Arc<Playback>,
    commands: Receiver<Command>,
    errors: Sender<String>,
    jobs: SyncSender<Job>,
    /// Commande reçue pendant une attente, à traiter au prochain tour.
    pending: Option<Command>,
    max_frames: usize,
    max_audio: usize,
}

/// Issue du traitement d'un paquet.
enum Flow {
    Continue,
    Interrupted,
    Quit,
}

impl Worker {
    fn run(mut self) {
        let mut generation = 0;
        // Après un déplacement, on décode sans restituer jusqu'à cette position.
        let mut skip_until: Option<f64> = None;
        let mut at_eof = false;
        // Créé dans ce thread : le contexte swresample n'est pas `Send`.
        let mut resampler: Option<Resampler> = None;

        loop {
            let command = if let Some(c) = self.pending.take() {
                Some(c)
            } else if at_eof {
                // En fin de fichier, on attend sans consommer de CPU.
                match self.commands.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                }
            } else {
                match self.commands.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            };

            if let Some(Command::Seek { target, generation: g }) = command {
                generation = g;
                let ts = ((target + self.start_time).max(0.0) * f64::from(ffi::AV_TIME_BASE)) as i64;
                // Se place sur l'image clé précédant `ts`.
                if let Err(e) = self.input.seek(ts, ..ts) {
                    let _ = self.errors.send(format!("échec du déplacement : {e}"));
                }
                self.video.decoder.flush();
                self.video.done = false;
                if let Some(audio) = &mut self.audio {
                    audio.decoder.flush();
                }
                resampler = None;
                skip_until = Some(target);
                at_eof = false;
                continue;
            }

            let mut packet = ffmpeg::Packet::empty();
            let flow = match packet.read(&mut self.input) {
                Ok(()) if packet.stream() == self.video.index => {
                    // Un paquet corrompu n'interrompt pas la lecture.
                    let _ = self.video.decoder.send_packet(&packet);
                    self.drain_video(generation, &mut skip_until)
                }
                Ok(()) => {
                    if let Some(audio) = &mut self.audio
                        && packet.stream() == audio.index
                    {
                        let _ = audio.decoder.send_packet(&packet);
                        self.drain_audio(generation, skip_until, &mut resampler);
                    }
                    Flow::Continue
                }
                Err(_) => {
                    // Fin de fichier (ou erreur de lecture) : on vide les décodeurs.
                    at_eof = true;
                    let _ = self.video.decoder.send_eof();
                    if let Some(audio) = &mut self.audio {
                        let _ = audio.decoder.send_eof();
                    }
                    self.drain_audio(generation, skip_until, &mut resampler);
                    let flow = self.drain_video(generation, &mut skip_until);
                    if matches!(flow, Flow::Continue) {
                        self.video.done = true;
                        self.playback.mark_end_of_stream(generation);
                    }
                    flow
                }
            };
            match flow {
                Flow::Continue => {}
                Flow::Interrupted => at_eof = false,
                Flow::Quit => return,
            }
        }
    }

    /// Récupère les images décodées et les confie au pool de conversion.
    fn drain_video(&mut self, generation: u64, skip_until: &mut Option<f64>) -> Flow {
        loop {
            let mut decoded = frame::Video::empty();
            if self.video.decoder.receive_frame(&mut decoded).is_err() {
                return Flow::Continue;
            }
            let pts = match decoded.timestamp() {
                Some(t) => t as f64 * self.video.time_base - self.start_time,
                None => self.video.next_pts,
            };
            self.video.next_pts = pts + self.video.frame_duration;

            if let Some(target) = *skip_until {
                if pts + self.video.frame_duration / 2.0 < target {
                    continue;
                }
                *skip_until = None;
            }

            // Attend qu'il y ait de la place, en restant attentif aux commandes.
            loop {
                match self.playback.wait_for_room(self.max_frames, self.max_audio, self.video.done) {
                    Room::Available => break,
                    Room::Full => match self.commands.try_recv() {
                        Ok(c) => {
                            self.pending = Some(c);
                            return Flow::Interrupted;
                        }
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => return Flow::Quit,
                    },
                }
            }

            if self.jobs.send(Job { generation, pts, frame: decoded }).is_err() {
                return Flow::Quit;
            }
        }
    }

    /// Récupère l'audio décodé, le rééchantillonne et le met en file.
    fn drain_audio(&mut self, generation: u64, skip_until: Option<f64>, resampler: &mut Option<Resampler>) {
        let Some(audio) = &mut self.audio else { return };
        let mut decoded = frame::Audio::empty();
        while audio.decoder.receive_frame(&mut decoded).is_ok() {
            let mut pts = match decoded.timestamp() {
                Some(t) => t as f64 * audio.time_base - self.start_time,
                None => audio.next_pts,
            };
            let mut samples = match resample(resampler, &decoded, audio.out_rate, audio.out_channels) {
                Ok(s) => s,
                Err(e) => {
                    let _ = self.errors.send(format!("rééchantillonnage audio : {e}"));
                    continue;
                }
            };
            let channels = usize::from(audio.out_channels);
            let rate = f64::from(audio.out_rate);
            audio.next_pts = pts + (samples.len() / channels) as f64 / rate;

            // Déplacement précis : on retire ce qui précède la cible.
            if let Some(target) = skip_until {
                if audio.next_pts <= target {
                    continue;
                }
                if pts < target {
                    let skip = (((target - pts) * rate) as usize * channels).min(samples.len());
                    samples.drain(..skip);
                    pts = target;
                }
            }
            self.playback.push_audio(generation, pts, &samples);
        }
    }
}

/// Thread du pool de conversion : rapatrie les images du GPU si besoin, les
/// convertit en RGBA et les dépose dans la file d'affichage.
fn convert_worker(
    jobs: Arc<Mutex<Receiver<Job>>>,
    hw: Option<HwConfig>,
    playback: Arc<Playback>,
    errors: Sender<String>,
) {
    let mut scaler = None;
    let mut downloaded = frame::Video::empty();
    loop {
        let job = match jobs.lock().map(|rx| rx.recv()) {
            Ok(Ok(job)) => job,
            _ => return, // décodeur arrêté
        };
        if !playback.is_current(job.generation) {
            continue;
        }
        let result = (|| -> Result<(egui::ColorImage, bool)> {
            let hardware = match &hw {
                Some(hw) => hwaccel::download(hw, &job.frame, &mut downloaded)?,
                None => false,
            };
            let src = if hardware { &downloaded } else { &job.frame };
            Ok((convert(&mut scaler, src, playback.target_size())?, hardware))
        })();
        match result {
            Ok((image, hardware)) => playback.push_frame(VideoFrame {
                generation: job.generation,
                pts: job.pts,
                image,
                hardware,
            }),
            Err(e) => {
                let _ = errors.send(format!("conversion d'image : {e}"));
            }
        }
    }
}

/// Convertit une image décodée (YUV…) en image RGBA egui, réduite à la
/// taille d'affichage `target` si celle-ci est plus petite (inutile de
/// convertir puis transférer une image 4K affichée dans une petite fenêtre).
fn convert(scaler: &mut Option<Scaler>, src: &frame::Video, target: (u32, u32)) -> Result<egui::ColorImage> {
    let (src_w, src_h) = (src.width(), src.height());
    let fit = |native: u32, wanted: u32| {
        // Arrondi à 16 px pour ne pas recréer le contexte à chaque pixel de
        // redimensionnement de la fenêtre.
        if wanted == 0 { native } else { native.min(wanted.div_ceil(16) * 16).max(16) }
    };
    let (dst_w, dst_h) = (fit(src_w, target.0), fit(src_h, target.1));
    let key = (src.format(), src_w, src_h, dst_w, dst_h);
    if scaler.as_ref().map(|s| s.key) != Some(key) {
        let mut context = scaling::Context::get(
            key.0, src_w, src_h,
            Pixel::RGBA, dst_w, dst_h,
            scaling::Flags::BILINEAR,
        )?;
        configure_colorspace(&mut context, src);
        *scaler = Some(Scaler { context, key, output: frame::Video::empty() });
    }
    let scaler = scaler.as_mut().unwrap();
    scaler.context.run(src, &mut scaler.output)?;

    // Une seule copie, ligne par ligne (FFmpeg aligne les lignes : stride ≥
    // largeur × 4), directement dans les pixels de l'image egui.
    let (width, height) = (dst_w as usize, dst_h as usize);
    let stride = scaler.output.stride(0);
    let data = scaler.output.data(0);
    let row = width * 4;
    let mut pixels = Vec::<egui::Color32>::with_capacity(width * height);
    unsafe {
        let dst = pixels.as_mut_ptr() as *mut u8;
        for y in 0..height {
            ptr::copy_nonoverlapping(data[y * stride..].as_ptr(), dst.add(y * row), row);
        }
        // `Color32` est un `[u8; 4]` : chaque pixel vient d'être écrit.
        pixels.set_len(width * height);
    }
    Ok(egui::ColorImage::new([width, height], pixels))
}

/// Choisit la matrice YUV→RGB (BT.601 / BT.709 / BT.2020) et la plage
/// (limitée / complète) indiquées par la vidéo, pour des couleurs correctes.
fn configure_colorspace(context: &mut scaling::Context, src: &frame::Video) {
    let space = match src.color_space() {
        color::Space::BT709 => ffi::SWS_CS_ITU709,
        color::Space::BT470BG | color::Space::SMPTE170M => ffi::SWS_CS_ITU601,
        color::Space::BT2020NCL | color::Space::BT2020CL => ffi::SWS_CS_BT2020,
        // Non précisé : convention habituelle selon la résolution.
        _ if src.height() >= 720 => ffi::SWS_CS_ITU709,
        _ => ffi::SWS_CS_ITU601,
    };
    let full_range = i32::from(src.color_range() == color::Range::JPEG);
    unsafe {
        ffi::sws_setColorspaceDetails(
            context.as_mut_ptr(),
            ffi::sws_getCoefficients(space),
            full_range,
            ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT),
            1,
            0,
            1 << 16,
            1 << 16,
        );
    }
}

/// Convertit une trame audio en échantillons `f32` entrelacés au format de
/// la carte son (fréquence et nombre de canaux).
fn resample(
    resampler: &mut Option<Resampler>,
    src: &frame::Audio,
    out_rate: u32,
    out_channels: u16,
) -> Result<Vec<f32>> {
    let key = (src.format(), src.rate(), src.channels());
    if resampler.as_ref().map(|r| r.key) != Some(key) {
        let mut layout = src.channel_layout();
        if layout.0.order == ffi::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC {
            layout = ChannelLayout::default(i32::from(src.channels()));
        }
        let context = resampling::Context::get(
            src.format(),
            layout,
            src.rate(),
            Sample::F32(ffmpeg::util::format::sample::Type::Packed),
            ChannelLayout::default(i32::from(out_channels)),
            out_rate,
        )?;
        *resampler = Some(Resampler { context, key });
    }
    let context = &mut resampler.as_mut().unwrap().context;
    let channels = usize::from(out_channels);
    unsafe {
        let ctx = context.as_mut_ptr();
        let in_samples = src.samples() as i32;
        let capacity = ffi::swr_get_out_samples(ctx, in_samples).max(0);
        let mut out = vec![0.0f32; capacity as usize * channels];
        let out_ptr = out.as_mut_ptr() as *mut u8;
        let written = ffi::swr_convert(
            ctx,
            &out_ptr as *const *mut u8 as _,
            capacity,
            (*src.as_ptr()).extended_data as _,
            in_samples,
        );
        if written < 0 {
            return Err(ffmpeg::Error::from(written).into());
        }
        out.truncate(written as usize * channels);
        Ok(out)
    }
}
