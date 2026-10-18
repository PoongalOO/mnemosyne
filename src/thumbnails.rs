//! Miniatures affichées au survol de la frise. Un décodeur logiciel séparé,
//! dans son propre thread, produit l'image la plus proche de la position
//! demandée ; seule la dernière demande compte (les précédentes sont
//! abandonnées dès qu'une nouvelle arrive).

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use eframe::egui;
use ffmpeg_next as ffmpeg;
use ffmpeg::{codec, ffi, format, frame, media};

use crate::decoder::{self, Scaler};

/// Largeur des miniatures produites.
const WIDTH: u32 = 240;
/// Temps de décodage maximal par miniature : au-delà, on garde l'image la
/// plus proche déjà obtenue (vidéos 4K à longs intervalles d'images clés).
const BUDGET: Duration = Duration::from_millis(400);

pub struct Thumbnailer {
    requests: Sender<f64>,
    results: Receiver<(f64, egui::ColorImage)>,
}

impl Thumbnailer {
    pub fn start(path: &Path) -> Self {
        let (requests, request_rx) = mpsc::channel();
        let (result_tx, results) = mpsc::channel();
        let path = path.to_path_buf();
        thread::spawn(move || {
            let _ = run(&path, &request_rx, &result_tx);
        });
        Self { requests, results }
    }

    /// Demande la miniature de la position `t` (en secondes).
    pub fn request(&self, t: f64) {
        let _ = self.requests.send(t);
    }

    /// Miniatures prêtes depuis le dernier appel.
    pub fn poll(&self) -> impl Iterator<Item = (f64, egui::ColorImage)> + '_ {
        self.results.try_iter()
    }
}

enum Next {
    Request(f64),
    Nothing,
    Quit,
}

/// Dernière demande en attente, sans bloquer.
fn latest(requests: &Receiver<f64>) -> Next {
    let mut next = Next::Nothing;
    loop {
        match requests.try_recv() {
            Ok(t) => next = Next::Request(t),
            Err(TryRecvError::Empty) => return next,
            Err(TryRecvError::Disconnected) => return Next::Quit,
        }
    }
}

fn run(path: &Path, requests: &Receiver<f64>, results: &Sender<(f64, egui::ColorImage)>) -> Result<()> {
    let mut input = format::input(&path)?;
    let start_time = unsafe {
        let t = (*input.as_ptr()).start_time;
        if t != ffi::AV_NOPTS_VALUE { t as f64 / f64::from(ffi::AV_TIME_BASE) } else { 0.0 }
    };
    let stream = input.streams().best(media::Type::Video).ok_or_else(|| anyhow!("aucun flux vidéo"))?;
    let (index, time_base) = (stream.index(), f64::from(stream.time_base()));
    let mut context = codec::context::Context::from_parameters(stream.parameters())?;
    // Peu de threads : la lecture reste prioritaire.
    context.set_threading(codec::threading::Config { kind: codec::threading::Type::Slice, count: 2 });
    let mut decoder = context.decoder().video()?;
    let mut scaler: Option<Scaler> = None;
    let (mut decoded, mut best) = (frame::Video::empty(), frame::Video::empty());
    let mut pending = None;

    'requests: loop {
        let mut target = match pending.take() {
            Some(t) => t,
            None => match requests.recv() {
                Ok(t) => t,
                Err(_) => return Ok(()),
            },
        };
        match latest(requests) {
            Next::Request(t) => target = t,
            Next::Nothing => {}
            Next::Quit => return Ok(()),
        }

        // Image clé précédant la position, puis décodage jusqu'à elle.
        let ts = ((target + start_time).max(0.0) * f64::from(ffi::AV_TIME_BASE)) as i64;
        if input.seek(ts, ..ts).is_err() {
            continue;
        }
        decoder.flush();
        let started = Instant::now();
        let mut found = false;
        let mut packet = ffmpeg::Packet::empty();
        loop {
            match latest(requests) {
                Next::Request(t) => {
                    pending = Some(t);
                    continue 'requests;
                }
                Next::Nothing => {}
                Next::Quit => return Ok(()),
            }
            let eof = packet.read(&mut input).is_err();
            if eof {
                let _ = decoder.send_eof();
            } else if packet.stream() != index {
                continue;
            } else {
                let _ = decoder.send_packet(&packet);
            }
            let mut done = eof;
            while decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.timestamp().or(decoded.pts()).map(|t| t as f64 * time_base - start_time);
                std::mem::swap(&mut decoded, &mut best);
                found = true;
                if pts.is_none_or(|p| p >= target - 0.02) || started.elapsed() > BUDGET {
                    done = true;
                    break;
                }
            }
            if done {
                break;
            }
        }
        if found && best.width() > 0 {
            let height = (WIDTH * best.height() / best.width()).max(1);
            if let Ok(image) = decoder::convert(&mut scaler, &best, (WIDTH, height))
                && results.send((target, image)).is_err()
            {
                return Ok(());
            }
        }
    }
}
