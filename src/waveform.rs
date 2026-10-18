//! Forme d'onde de la piste audio, calculée en arrière-plan : niveau crête
//! par tranche de 20 ms, affiché sous la frise de montage.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use anyhow::Result;
use ffmpeg_next as ffmpeg;
use ffmpeg::{codec, ffi, format, frame, media};

use crate::decoder::{self, Resampler};

/// Durée d'une tranche, en secondes.
pub const BUCKET: f64 = 0.02;
/// Fréquence (mono) à laquelle l'audio est analysé.
const RATE: u32 = 8000;

#[derive(Default)]
pub struct Peaks {
    /// Crête de chaque tranche (0 à 1).
    pub values: Vec<f32>,
    /// Plus grande crête rencontrée (pour normaliser l'affichage).
    pub max: f32,
    /// Analyse terminée.
    pub done: bool,
}

/// Analyse en cours ou terminée. Le thread s'arrête si elle est abandonnée.
pub struct Waveform {
    peaks: Arc<Mutex<Peaks>>,
}

impl Waveform {
    pub fn start(path: &Path) -> Self {
        let peaks = Arc::new(Mutex::new(Peaks::default()));
        let shared = peaks.clone();
        let path = path.to_path_buf();
        thread::spawn(move || {
            let _ = run(&path, &shared);
            shared.lock().unwrap().done = true;
        });
        Self { peaks }
    }

    pub fn peaks(&self) -> MutexGuard<'_, Peaks> {
        self.peaks.lock().unwrap()
    }
}

fn run(path: &Path, peaks: &Arc<Mutex<Peaks>>) -> Result<()> {
    let mut input = format::input(&path)?;
    let start_time = unsafe {
        let t = (*input.as_ptr()).start_time;
        if t != ffi::AV_NOPTS_VALUE { t as f64 / f64::from(ffi::AV_TIME_BASE) } else { 0.0 }
    };
    let Some(stream) = input.streams().best(media::Type::Audio) else { return Ok(()) };
    let (index, time_base) = (stream.index(), f64::from(stream.time_base()));
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())?.decoder().audio()?;
    let mut resampler: Option<Resampler> = None;
    let mut decoded = frame::Audio::empty();
    // Position de la prochaine trame si elle n'a pas d'horodatage.
    let mut next = 0.0;

    let mut drain = |decoder: &mut codec::decoder::Audio| -> Result<bool> {
        while decoder.receive_frame(&mut decoded).is_ok() {
            let pts = decoded.timestamp().or(decoded.pts()).map_or(next, |t| t as f64 * time_base - start_time);
            let samples = decoder::resample(&mut resampler, &decoded, RATE, 1)?;
            next = pts + samples.len() as f64 / f64::from(RATE);
            let mut peaks = peaks.lock().unwrap();
            for (i, s) in samples.iter().enumerate() {
                let t = pts + i as f64 / f64::from(RATE);
                if t < 0.0 {
                    continue;
                }
                let bucket = (t / BUCKET) as usize;
                if bucket >= peaks.values.len() {
                    peaks.values.resize(bucket + 1, 0.0);
                }
                let v = s.abs().min(1.0);
                if v > peaks.values[bucket] {
                    peaks.values[bucket] = v;
                    peaks.max = peaks.max.max(v);
                }
            }
        }
        // Plus personne ne regarde : on arrête.
        Ok(Arc::strong_count(peaks) > 1)
    };

    for (stream, packet) in input.packets() {
        if stream.index() != index {
            continue;
        }
        let _ = decoder.send_packet(&packet);
        if !drain(&mut decoder)? {
            return Ok(());
        }
    }
    let _ = decoder.send_eof();
    drain(&mut decoder)?;
    Ok(())
}
