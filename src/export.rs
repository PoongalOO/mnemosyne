//! Enregistrement de la vidéo montée : les passages conservés sont mis bout
//! à bout dans un nouveau fichier. Le fichier d'origine n'est jamais modifié :
//! l'écriture se fait dans un fichier temporaire `.part`, renommé à la fin.
//!
//! Deux modes :
//! - **précis** : réencodage (H.264 → libx264, H.265 → libx265, son en AAC
//!   ou MP3 pour l'AVI). Coupes à l'image et à l'échantillon près ;
//! - **rapide** : copie des paquets sans réencodage. Quasi instantané et sans
//!   perte, mais les coupes sont calées sur les images clés.

use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, bail, Context as _, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::{
    codec, encoder, ffi, format, frame, media, picture,
    software::{resampling, scaling},
    util::channel_layout::ChannelLayout,
    Dictionary, Packet, Rational,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Precise,
    Fast,
}

/// Enregistrement en cours dans un thread dédié.
pub struct Export {
    progress: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    result: Receiver<Result<(), String>>,
    pub destination: PathBuf,
}

/// Message d'erreur utilisé quand l'utilisateur annule.
pub const CANCELLED: &str = "enregistrement annulé";

impl Export {
    /// Lance l'enregistrement des passages `segments` (en secondes depuis le
    /// début de la vidéo) de `source` vers `destination`.
    pub fn start(source: &Path, destination: &Path, segments: Vec<(f64, f64)>, mode: Mode) -> Result<Self> {
        if same_file(source, destination) {
            bail!("la destination est le fichier d'origine : choisissez un autre nom");
        }
        if segments.is_empty() {
            bail!("tout a été supprimé : il n'y a rien à enregistrer");
        }
        let progress = Arc::new(AtomicU64::new(0f64.to_bits()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: source.to_owned(),
            destination: destination.to_owned(),
            segments,
            mode,
            progress: progress.clone(),
            cancel: cancel.clone(),
        };
        thread::Builder::new()
            .name("enregistrement".into())
            .spawn(move || {
                let _ = tx.send(job.run().map_err(|e| format!("{e:#}")));
            })?;
        Ok(Self { progress, cancel, result: rx, destination: destination.to_owned() })
    }

    /// Avancement entre 0 et 1.
    pub fn progress(&self) -> f32 {
        f64::from_bits(self.progress.load(Ordering::Relaxed)) as f32
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Résultat, une fois l'enregistrement terminé.
    pub fn poll(&self) -> Option<Result<(), String>> {
        self.result.try_recv().ok()
    }
}

/// Vrai si les deux chemins désignent le même fichier.
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Positions (s, depuis le début de la vidéo) des images clés du flux vidéo.
/// Lit seulement les en-têtes des paquets, sans décoder.
pub fn keyframes(source: &Path) -> Result<Vec<f64>> {
    let mut ictx = format::input(&source)?;
    let start_time = unsafe {
        let t = (*ictx.as_ptr()).start_time;
        if t != ffi::AV_NOPTS_VALUE { t as f64 / f64::from(ffi::AV_TIME_BASE) } else { 0.0 }
    };
    let stream = ictx.streams().best(media::Type::Video).ok_or_else(|| anyhow!("aucun flux vidéo"))?;
    let (index, tb) = (stream.index(), f64::from(stream.time_base()));
    let mut keys = Vec::new();
    for (stream, packet) in ictx.packets() {
        if stream.index() == index && packet.is_key()
            && let Some(ts) = packet.pts().or(packet.dts())
        {
            keys.push(ts as f64 * tb - start_time);
        }
    }
    keys.sort_by(f64::total_cmp);
    Ok(keys)
}

/// Passages réellement copiés par le mode rapide : chaque début recule à
/// l'image clé précédente (sans revenir avant la fin du passage précédent),
/// chaque fin avance à l'image clé suivante.
pub fn fast_segments(segments: &[(f64, f64)], keys: &[f64], duration: f64) -> Vec<(f64, f64)> {
    const EPS: f64 = 1e-6;
    let mut result: Vec<(f64, f64)> = Vec::new();
    let mut previous_end = f64::NEG_INFINITY;
    for &(s, e) in segments {
        let before = keys.iter().copied().rfind(|&k| k <= s + EPS).unwrap_or(0.0);
        let Some(start) = (if before >= previous_end - EPS {
            Some(before)
        } else {
            keys.iter().copied().find(|&k| k >= previous_end - EPS)
        }) else {
            break; // plus d'image clé : rien de plus à copier
        };
        let end = keys.iter().copied().find(|&k| k >= e - EPS && k > start).unwrap_or(duration);
        if end > start {
            // Passage contigu au précédent : la coupe disparaît.
            match result.last_mut() {
                Some(last) if (last.1 - start).abs() < EPS => last.1 = end,
                _ => result.push((start, end)),
            }
            previous_end = end;
        }
    }
    result
}

/// Muxer FFmpeg correspondant à l'extension du fichier de destination.
pub fn muxer_for(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" => "mp4",
        "mov" => "mov",
        "avi" => "avi",
        // Le muxer WebM n'accepte pas H.264/H.265 : on écrit du Matroska.
        _ => "matroska",
    }
}

struct Job {
    source: PathBuf,
    destination: PathBuf,
    segments: Vec<(f64, f64)>,
    mode: Mode,
    progress: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
}

/// Flux d'entrée retenus.
struct Inputs {
    ictx: format::context::Input,
    start_time: f64,
    video: usize,
    audio: Option<usize>,
}

impl Job {
    fn run(self) -> Result<()> {
        let mut name = self.destination.file_name().unwrap_or_default().to_owned();
        name.push(".part");
        let part = self.destination.with_file_name(name);

        let result = self.write(&part);
        match result {
            Ok(()) if !self.cancelled() => {
                fs::rename(&part, &self.destination).context("impossible de renommer le fichier")?;
                self.set_progress(1.0);
                Ok(())
            }
            Ok(()) => {
                let _ = fs::remove_file(&part);
                bail!(CANCELLED)
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                if self.cancelled() { bail!(CANCELLED) } else { Err(e) }
            }
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn check_cancel(&self) -> Result<()> {
        if self.cancelled() { bail!(CANCELLED) } else { Ok(()) }
    }

    fn set_progress(&self, value: f64) {
        self.progress.store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    fn total(&self) -> f64 {
        self.segments.iter().map(|(s, e)| e - s).sum::<f64>().max(1e-6)
    }

    fn write(&self, part: &Path) -> Result<()> {
        ffmpeg::init()?;
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Error);
        let ictx = format::input(&self.source)
            .with_context(|| format!("impossible d'ouvrir « {} »", self.source.display()))?;
        let start_time = unsafe {
            let t = (*ictx.as_ptr()).start_time;
            if t != ffi::AV_NOPTS_VALUE { t as f64 / f64::from(ffi::AV_TIME_BASE) } else { 0.0 }
        };
        let video = ictx
            .streams()
            .best(media::Type::Video)
            .ok_or_else(|| anyhow!("aucun flux vidéo"))?
            .index();
        let audio = ictx.streams().best(media::Type::Audio).map(|s| s.index());
        let inputs = Inputs { ictx, start_time, video, audio };

        let muxer = muxer_for(&self.destination);
        let octx = format::output_as(&part, muxer)
            .with_context(|| format!("impossible de créer « {} »", part.display()))?;

        match self.mode {
            Mode::Fast => self.remux(inputs, octx),
            Mode::Precise => self.transcode(inputs, octx, muxer),
        }
    }

    // ---- Mode rapide : copie des paquets ------------------------------------

    fn remux(&self, mut inp: Inputs, mut octx: format::context::Output) -> Result<()> {
        let streams: Vec<usize> = std::iter::once(inp.video).chain(inp.audio).collect();
        let mut in_tb = Vec::new();
        for &index in &streams {
            let ist = inp.ictx.stream(index).unwrap();
            let mut ost = octx.add_stream(encoder::find(codec::Id::None))?;
            ost.set_parameters(ist.parameters());
            // Laisse le muxer choisir l'identifiant du codec (conteneur différent).
            unsafe { (*ost.parameters().as_mut_ptr()).codec_tag = 0 };
            in_tb.push(ist.time_base());
        }
        octx.write_header().context("le conteneur choisi n'accepte pas ces codecs")?;
        let out_tb: Vec<Rational> =
            (0..streams.len()).map(|i| octx.stream(i).unwrap().time_base()).collect();

        let total = self.total();
        let mut offset = 0.0; // position de sortie (s)
        let mut last_dts: Vec<Option<i64>> = vec![None; streams.len()];
        let mut previous_end = f64::NEG_INFINITY; // fin du segment précédent (s, source)

        for &(s, e) in &self.segments {
            let (s_abs, e_abs) = (s + inp.start_time, e + inp.start_time);
            let ts = (s_abs * f64::from(ffi::AV_TIME_BASE)) as i64;
            inp.ictx.seek(ts, ..ts)?;

            let mut origin: Option<f64> = None; // première image clé retenue
            let mut video_end: Option<f64> = None; // image clé qui termine le segment
            let mut last_video_end: f64 = 0.0;
            loop {
                self.check_cancel()?;
                let mut packet = Packet::empty();
                if packet.read(&mut inp.ictx).is_err() {
                    break;
                }
                let Some(out) = streams.iter().position(|&i| i == packet.stream()) else { continue };
                let tb = f64::from(in_tb[out]);
                let Some(ts) = packet.pts().or(packet.dts()) else { continue };
                let t = ts as f64 * tb;

                if out == 0 {
                    match origin {
                        None => {
                            // Le segment commence à une image clé, sans revenir
                            // sur ce qui a déjà été copié.
                            if !packet.is_key() || t < previous_end - 1e-6 {
                                continue;
                            }
                            origin = Some(t);
                        }
                        Some(o) if packet.is_key() && t >= e_abs - 1e-6 && t > o => {
                            video_end = Some(t);
                        }
                        // Image « de tête » d'un GOP ouvert (affichée avant
                        // l'image clé, dépendant du GOP précédent) :
                        // indécodable sans lui, on ne la copie pas.
                        Some(o) if t < o - 1e-6 => continue,
                        _ => {}
                    }
                    if video_end.is_some() {
                        if inp.audio.is_none() {
                            break;
                        }
                        continue;
                    }
                    last_video_end = last_video_end.max(t + packet.duration() as f64 * tb);
                    self.set_progress((offset + t - origin.unwrap()) / total);
                } else {
                    let Some(o) = origin else { continue };
                    if t < o {
                        continue;
                    }
                    if video_end.is_some_and(|end| t >= end) {
                        break;
                    }
                    if video_end.is_none() && t >= e_abs + 5.0 {
                        break; // sécurité : pas d'image clé de fin trouvée
                    }
                }

                // Décale les horodatages sur la ligne de temps de sortie.
                let shift = ((offset - origin.unwrap()) / tb).round() as i64;
                packet.set_pts(packet.pts().map(|v| v + shift));
                packet.set_dts(packet.dts().map(|v| v + shift));
                packet.rescale_ts(in_tb[out], out_tb[out]);
                // DTS strictement croissants. Après un déplacement, le
                // démultiplexeur Matroska ne connaît pas le DTS du premier
                // paquet : on prend la plus petite valeur valide.
                let dts = match (packet.dts(), last_dts[out]) {
                    (Some(d), Some(last)) => d.max(last + 1),
                    (Some(d), None) => d,
                    (None, Some(last)) => last + 1,
                    (None, None) => packet.pts().unwrap_or(0),
                };
                packet.set_dts(Some(dts));
                packet.set_pts(Some(packet.pts().unwrap_or(dts).max(dts)));
                last_dts[out] = Some(dts);
                packet.set_position(-1);
                packet.set_stream(out);
                packet.write_interleaved(&mut octx)?;
            }

            if let Some(o) = origin {
                let end = video_end.unwrap_or(last_video_end);
                offset += (end - o).max(0.0);
                previous_end = end;
            }
        }
        octx.write_trailer()?;
        Ok(())
    }

    // ---- Mode précis : réencodage --------------------------------------------

    fn transcode(&self, mut inp: Inputs, mut octx: format::context::Output, muxer: &str) -> Result<()> {
        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);
        let mut video = VideoTranscoder::new(&inp, &mut octx, global_header, muxer)?;
        let mut audio = match inp.audio {
            Some(index) => Some(AudioTranscoder::new(&inp, index, &mut octx, global_header, muxer)?),
            None => None,
        };

        octx.write_header().context("le conteneur choisi n'accepte pas ces codecs")?;
        video.out_tb = octx.stream(video.ost).unwrap().time_base();
        if let Some(a) = &mut audio {
            a.out_tb = octx.stream(a.ost).unwrap().time_base();
        }

        let total = self.total();
        let mut offset = 0.0;
        for &(s, e) in &self.segments {
            let seg = Segment { start: s + inp.start_time, end: e + inp.start_time, offset };
            let ts = (seg.start * f64::from(ffi::AV_TIME_BASE)) as i64;
            inp.ictx.seek(ts, ..ts)?;
            video.decoder.flush();
            video.done = false;
            if let Some(a) = &mut audio {
                a.begin_segment(&seg);
            }

            loop {
                self.check_cancel()?;
                let mut packet = Packet::empty();
                let eof = packet.read(&mut inp.ictx).is_err();
                if eof {
                    let _ = video.decoder.send_eof();
                    if let Some(a) = &mut audio {
                        let _ = a.decoder.send_eof();
                    }
                } else if packet.stream() == inp.video {
                    let _ = video.decoder.send_packet(&packet);
                } else if let Some(a) = audio.as_mut().filter(|_| Some(packet.stream()) == inp.audio) {
                    let _ = a.decoder.send_packet(&packet);
                } else {
                    continue;
                }

                if let Some(t) = video.drain(&seg, &mut octx)? {
                    self.set_progress((offset + t - seg.start) / total);
                }
                if let Some(a) = &mut audio {
                    a.drain(&seg, &mut octx)?;
                }
                if eof || (video.done && audio.as_ref().is_none_or(|a| a.done)) {
                    break;
                }
            }
            offset += e - s;
            if let Some(a) = &mut audio {
                // Comble un éventuel manque de son pour garder la synchronisation.
                a.pad_to(offset)?;
            }
        }

        video.finish(&mut octx)?;
        if let Some(a) = &mut audio {
            a.finish(&mut octx)?;
        }
        octx.write_trailer()?;
        Ok(())
    }
}

/// Passage à copier : bornes dans la source (s) et position dans la sortie.
struct Segment {
    start: f64,
    end: f64,
    offset: f64,
}

/// Écrit les paquets produits par un encodeur.
fn write_packets(
    encoder: &mut encoder::Encoder,
    ost: usize,
    enc_tb: Rational,
    out_tb: Rational,
    octx: &mut format::context::Output,
) -> Result<()> {
    let mut packet = Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(ost);
        packet.rescale_ts(enc_tb, out_tb);
        packet.write_interleaved(octx)?;
    }
    Ok(())
}

struct VideoTranscoder {
    decoder: ffmpeg::decoder::Video,
    encoder: encoder::Video,
    ost: usize,
    in_tb: f64,
    enc_tb: Rational,
    out_tb: Rational,
    frame_duration: f64,
    last_pts: i64,
    /// Conversion de format de pixel si l'encodeur ne gère pas celui de la source.
    scaler: Option<scaling::Context>,
    done: bool,
}

impl VideoTranscoder {
    fn new(inp: &Inputs, octx: &mut format::context::Output, global_header: bool, muxer: &str) -> Result<Self> {
        let ist = inp.ictx.stream(inp.video).unwrap();
        let mut context = codec::context::Context::from_parameters(ist.parameters())?;
        context.set_threading(codec::threading::Config {
            kind: codec::threading::Type::Frame,
            count: 0,
        });
        let decoder = context.decoder().video()?;

        let (name, options): (&str, &[(&str, &str)]) = match decoder.id() {
            codec::Id::HEVC => ("libx265", &[("crf", "20"), ("preset", "fast"), ("x265-params", "log-level=error")]),
            _ => ("libx264", &[("crf", "18"), ("preset", "fast")]),
        };
        let codec = encoder::find_by_name(name)
            .ok_or_else(|| anyhow!("encodeur {name} indisponible dans cette version de FFmpeg"))?;
        let formats: Vec<_> = codec.video()?.formats().map(|f| f.collect()).unwrap_or_default();
        let format = if formats.is_empty() || formats.contains(&decoder.format()) {
            decoder.format()
        } else {
            formats[0]
        };

        let mut ost = octx.add_stream(codec)?;
        let ost_index = ost.index();
        let mut enc = codec::context::Context::new_with_codec(codec).encoder().video()?;
        enc.set_width(decoder.width());
        enc.set_height(decoder.height());
        enc.set_aspect_ratio(decoder.aspect_ratio());
        enc.set_format(format);
        let rate = ist.avg_frame_rate();
        if rate.numerator() > 0 && rate.denominator() > 0 {
            enc.set_frame_rate(Some(rate));
        }
        enc.set_time_base(ist.time_base());
        enc.set_colorspace(decoder.color_space());
        enc.set_color_range(decoder.color_range());
        unsafe {
            let (e, d) = (enc.as_mut_ptr(), decoder.as_ptr());
            (*e).color_primaries = (*d).color_primaries;
            (*e).color_trc = (*d).color_trc;
        }
        if global_header {
            enc.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let mut dict = Dictionary::new();
        for (k, v) in options {
            dict.set(k, v);
        }
        let encoder = enc.open_with(dict).with_context(|| format!("ouverture de l'encodeur {name}"))?;
        ost.set_parameters(&encoder);
        if muxer == "mp4" && name == "libx265" {
            // Étiquette « hvc1 » attendue par les lecteurs Apple.
            unsafe {
                (*ost.parameters().as_mut_ptr()).codec_tag =
                    u32::from_le_bytes(*b"hvc1");
            }
        }

        let frame_duration = if rate.numerator() > 0 { f64::from(rate.invert()) } else { 1.0 / 25.0 };
        Ok(Self {
            decoder,
            encoder,
            ost: ost_index,
            in_tb: f64::from(ist.time_base()),
            enc_tb: ist.time_base(),
            out_tb: ist.time_base(),
            frame_duration,
            last_pts: i64::MIN,
            scaler: None,
            done: false,
        })
    }

    /// Encode les images décodées appartenant au segment. Renvoie la position
    /// (source) de la dernière image traitée.
    fn drain(&mut self, seg: &Segment, octx: &mut format::context::Output) -> Result<Option<f64>> {
        let mut last = None;
        let mut decoded = frame::Video::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let Some(ts) = decoded.timestamp() else { continue };
            let t = ts as f64 * self.in_tb;
            let half = self.frame_duration / 2.0;
            if t < seg.start - half {
                continue;
            }
            if t >= seg.end - half {
                self.done = true;
                continue;
            }
            last = Some(t);

            let out_t = seg.offset + (t - seg.start).max(0.0);
            let mut pts = (out_t / f64::from(self.enc_tb)).round() as i64;
            if pts <= self.last_pts {
                pts = self.last_pts + 1;
            }
            self.last_pts = pts;

            let encoder_format = self.encoder.format();
            let mut frame = if decoded.format() != encoder_format {
                let scaler = match &mut self.scaler {
                    Some(s) => s,
                    None => self.scaler.insert(scaling::Context::get(
                        decoded.format(), decoded.width(), decoded.height(),
                        encoder_format, decoded.width(), decoded.height(),
                        scaling::Flags::BILINEAR,
                    )?),
                };
                let mut converted = frame::Video::empty();
                scaler.run(&decoded, &mut converted)?;
                converted
            } else {
                decoded.clone()
            };
            frame.set_pts(Some(pts));
            frame.set_kind(picture::Type::None);
            self.encoder.send_frame(&frame)?;
            write_packets(&mut self.encoder, self.ost, self.enc_tb, self.out_tb, octx)?;
        }
        Ok(last)
    }

    fn finish(&mut self, octx: &mut format::context::Output) -> Result<()> {
        self.encoder.send_eof()?;
        write_packets(&mut self.encoder, self.ost, self.enc_tb, self.out_tb, octx)
    }
}

struct AudioTranscoder {
    decoder: ffmpeg::decoder::Audio,
    encoder: encoder::Audio,
    ost: usize,
    in_tb: f64,
    enc_tb: Rational,
    out_tb: Rational,
    format: format::Sample,
    layout: ChannelLayout,
    rate: u32,
    frame_size: usize,
    resampler: Option<resampling::Context>,
    fifo: *mut ffi::AVAudioFifo,
    /// Échantillons écrits dans la file depuis le début (= position de sortie).
    queued: i64,
    /// Échantillons envoyés à l'encodeur (horodatage du prochain).
    encoded: i64,
    /// Position de sortie (échantillons) du début du segment courant.
    segment_start: i64,
    done: bool,
}

impl Drop for AudioTranscoder {
    fn drop(&mut self) {
        unsafe { ffi::av_audio_fifo_free(self.fifo) };
    }
}

impl AudioTranscoder {
    fn new(
        inp: &Inputs,
        index: usize,
        octx: &mut format::context::Output,
        global_header: bool,
        muxer: &str,
    ) -> Result<Self> {
        let ist = inp.ictx.stream(index).unwrap();
        let decoder = codec::context::Context::from_parameters(ist.parameters())?.decoder().audio()?;

        let name = if muxer == "avi" { "libmp3lame" } else { "aac" };
        let codec = encoder::find_by_name(name)
            .or_else(|| encoder::find_by_name("aac"))
            .ok_or_else(|| anyhow!("aucun encodeur audio disponible"))?;
        let caps = codec.audio()?;
        let formats: Vec<format::Sample> = caps.formats().map(|f| f.collect()).unwrap_or_default();
        let format = if formats.contains(&decoder.format()) || formats.is_empty() {
            decoder.format()
        } else {
            formats[0]
        };
        let rates: Vec<i32> = caps.rates().map(|r| r.collect()).unwrap_or_default();
        let src_rate = decoder.rate() as i32;
        let rate = if rates.is_empty() || rates.contains(&src_rate) {
            src_rate
        } else if rates.contains(&48000) {
            48000
        } else {
            rates[0]
        };
        let max_channels = if codec.name() == "libmp3lame" { 2 } else { 8 };
        let channels = i32::from(decoder.channels()).clamp(1, max_channels);
        let layout = ChannelLayout::default(channels);

        let mut ost = octx.add_stream(codec)?;
        let ost_index = ost.index();
        let mut enc = codec::context::Context::new_with_codec(codec).encoder().audio()?;
        enc.set_rate(rate);
        enc.set_format(format);
        enc.set_channel_layout(layout);
        enc.set_time_base(Rational(1, rate));
        enc.set_bit_rate(if channels > 2 { 384_000 } else { 192_000 });
        if global_header {
            enc.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let encoder = enc.open_with(Dictionary::new()).context("ouverture de l'encodeur audio")?;
        ost.set_parameters(&encoder);

        let frame_size = match encoder.frame_size() {
            0 => 1024,
            n => n as usize,
        };
        let fifo = unsafe { ffi::av_audio_fifo_alloc(format.into(), channels, frame_size as i32) };
        if fifo.is_null() {
            bail!("allocation de la file audio impossible");
        }
        Ok(Self {
            decoder,
            encoder,
            ost: ost_index,
            in_tb: f64::from(ist.time_base()),
            enc_tb: Rational(1, rate),
            out_tb: Rational(1, rate),
            format,
            layout,
            rate: rate as u32,
            frame_size,
            resampler: None,
            fifo,
            queued: 0,
            encoded: 0,
            segment_start: 0,
            done: false,
        })
    }

    fn begin_segment(&mut self, seg: &Segment) {
        self.decoder.flush();
        self.resampler = None;
        self.done = false;
        self.segment_start = (seg.offset * f64::from(self.rate)).round() as i64;
    }

    fn drain(&mut self, seg: &Segment, octx: &mut format::context::Output) -> Result<()> {
        let rate = f64::from(self.rate);
        let mut decoded = frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let Some(ts) = decoded.timestamp() else { continue };
            let t0 = ts as f64 * self.in_tb;
            let duration = decoded.samples() as f64 / f64::from(decoded.rate().max(1));
            if t0 + duration <= seg.start {
                continue;
            }
            if t0 >= seg.end {
                self.done = true;
                continue;
            }

            let (converted, count) = self.resample(&decoded)?;
            let first = (((seg.start - t0) * rate).round().max(0.0) as usize).min(count);
            let last = (((seg.end - t0) * rate).round().max(0.0) as usize).min(count);
            if last <= first {
                continue;
            }
            // Silence si le son commence après le début attendu.
            let expected = self.segment_start + ((t0.max(seg.start) - seg.start) * rate).round() as i64;
            self.pad_samples(expected - self.queued)?;
            self.queue(&converted, first, last - first)?;
            self.encode_queued(false, octx)?;
        }
        Ok(())
    }

    /// Convertit une trame au format de l'encodeur.
    fn resample(&mut self, src: &frame::Audio) -> Result<(frame::Audio, usize)> {
        if self.resampler.is_none() {
            let mut layout = src.channel_layout();
            if layout.0.order == ffi::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC {
                layout = ChannelLayout::default(i32::from(src.channels()));
            }
            self.resampler = Some(resampling::Context::get(
                src.format(), layout, src.rate(),
                self.format, self.layout, self.rate,
            )?);
        }
        let swr = self.resampler.as_mut().unwrap();
        unsafe {
            let ctx = swr.as_mut_ptr();
            let capacity = ffi::swr_get_out_samples(ctx, src.samples() as i32).max(1);
            let mut out = frame::Audio::new(self.format, capacity as usize, self.layout);
            let written = ffi::swr_convert(
                ctx,
                (*out.as_mut_ptr()).extended_data as _,
                capacity,
                (*src.as_ptr()).extended_data as _,
                src.samples() as i32,
            );
            if written < 0 {
                return Err(ffmpeg::Error::from(written).into());
            }
            Ok((out, written as usize))
        }
    }

    /// Ajoute `count` échantillons de `frame` (à partir de `first`) à la file.
    fn queue(&mut self, frame: &frame::Audio, first: usize, count: usize) -> Result<()> {
        unsafe {
            let fmt: ffi::AVSampleFormat = self.format.into();
            let bytes = ffi::av_get_bytes_per_sample(fmt) as usize;
            let channels = self.layout.channels() as usize;
            let planar = ffi::av_sample_fmt_is_planar(fmt) != 0;
            let data = (*frame.as_ptr()).extended_data;
            let planes: Vec<*mut c_void> = if planar {
                (0..channels).map(|c| (*data.add(c)).add(first * bytes) as *mut c_void).collect()
            } else {
                vec![(*data).add(first * bytes * channels) as *mut c_void]
            };
            if ffi::av_audio_fifo_write(self.fifo, planes.as_ptr() as *mut *mut c_void, count as i32) < count as i32 {
                bail!("écriture dans la file audio impossible");
            }
        }
        self.queued += count as i64;
        Ok(())
    }

    /// Ajoute `count` échantillons de silence.
    fn pad_samples(&mut self, count: i64) -> Result<()> {
        if count <= 0 {
            return Ok(());
        }
        let mut silence = frame::Audio::new(self.format, count as usize, self.layout);
        unsafe {
            ffi::av_samples_set_silence(
                (*silence.as_mut_ptr()).extended_data,
                0,
                count as i32,
                self.layout.channels(),
                self.format.into(),
            );
        }
        self.queue(&silence, 0, count as usize)
    }

    /// Complète le son jusqu'à la position de sortie `offset` (s).
    fn pad_to(&mut self, offset: f64) -> Result<()> {
        let target = (offset * f64::from(self.rate)).round() as i64;
        self.pad_samples(target - self.queued)
    }

    /// Envoie à l'encodeur des trames complètes (et le reste si `all`).
    fn encode_queued(&mut self, all: bool, octx: &mut format::context::Output) -> Result<()> {
        loop {
            let available = unsafe { ffi::av_audio_fifo_size(self.fifo) } as usize;
            let count = if available >= self.frame_size {
                self.frame_size
            } else if all && available > 0 {
                available
            } else {
                return Ok(());
            };
            let mut frame = frame::Audio::new(self.format, count, self.layout);
            frame.set_rate(self.rate);
            unsafe {
                ffi::av_audio_fifo_read(
                    self.fifo,
                    (*frame.as_mut_ptr()).extended_data as *mut *mut c_void,
                    count as i32,
                );
            }
            frame.set_pts(Some(self.encoded));
            self.encoded += count as i64;
            self.encoder.send_frame(&frame)?;
            write_packets(&mut self.encoder, self.ost, self.enc_tb, self.out_tb, octx)?;
        }
    }

    fn finish(&mut self, octx: &mut format::context::Output) -> Result<()> {
        self.encode_queued(true, octx)?;
        self.encoder.send_eof()?;
        write_packets(&mut self.encoder, self.ost, self.enc_tb, self.out_tb, octx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: [f64; 13] = [0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0, 5.5, 6.0];

    #[test]
    fn calage_sur_les_images_cles() {
        // Début coupé : recule à l'image clé précédente.
        assert_eq!(fast_segments(&[(1.2, 6.0)], &KEYS, 6.0), vec![(1.0, 6.0)]);
        // Fin coupée : avance à l'image clé suivante.
        assert_eq!(fast_segments(&[(0.0, 2.2)], &KEYS, 6.0), vec![(0.0, 2.5)]);
        // Coupes exactes sur des images clés.
        assert_eq!(fast_segments(&[(0.5, 1.0), (2.0, 3.0)], &KEYS, 6.0), vec![(0.5, 1.0), (2.0, 3.0)]);
        // Passage supprimé plus court que l'intervalle entre images clés : il disparaît.
        assert_eq!(fast_segments(&[(0.5, 1.2), (1.4, 2.6)], &KEYS, 6.0), vec![(0.5, 3.0)]);
        assert_eq!(fast_segments(&[(0.5, 1.2), (1.8, 2.6)], &KEYS, 6.0), vec![(0.5, 3.0)]);
    }

    #[test]
    fn une_seule_image_cle() {
        assert_eq!(fast_segments(&[(1.0, 2.0)], &[0.0], 8.0), vec![(0.0, 8.0)]);
        assert_eq!(fast_segments(&[(0.0, 1.0), (2.0, 3.0)], &[0.0], 8.0), vec![(0.0, 8.0)]);
    }

    #[test]
    fn extension_de_sortie() {
        assert_eq!(muxer_for(Path::new("a.MP4")), "mp4");
        assert_eq!(muxer_for(Path::new("a.avi")), "avi");
        assert_eq!(muxer_for(Path::new("a.webm")), "matroska");
        assert_eq!(muxer_for(Path::new("a.mkv")), "matroska");
    }
}
