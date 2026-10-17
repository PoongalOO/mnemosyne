//! État de lecture partagé entre l'interface, le thread de décodage et la
//! sortie audio : horloge maître, file d'images vidéo et tampon audio.
//!
//! Lorsqu'une sortie audio existe, c'est elle qui fait avancer l'horloge
//! (la vidéo se cale sur le son) ; sinon l'horloge suit le temps système.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::decoder::VideoFrame;

/// Durée maximale pendant laquelle l'horloge audio est extrapolée sans
/// nouvelle donnée de la carte son (au-delà, elle s'arrête : sous-alimentation).
const MAX_EXTRAPOLATION: f64 = 0.2;

pub struct Playback {
    state: Mutex<State>,
    /// Réveille le thread de décodage quand de la place se libère.
    wakeup: Condvar,
}

struct State {
    generation: u64,

    // Horloge : position `anchor_pts` à l'instant `anchor_at`.
    paused: bool,
    anchor_pts: f64,
    anchor_at: Instant,
    /// Vrai si la sortie audio pilote l'horloge.
    audio_clock: bool,
    /// La carte son a fourni une référence depuis le dernier déplacement.
    audio_started: bool,
    /// La première image a été affichée depuis le dernier déplacement.
    video_started: bool,

    /// Images triées par position (le pool de conversion peut les livrer
    /// dans le désordre).
    frames: VecDeque<VideoFrame>,
    /// Position de la dernière image affichée.
    last_shown: f64,
    /// Génération pour laquelle le décodeur a atteint la fin du fichier.
    eof_generation: Option<u64>,

    /// Échantillons entrelacés au format de la carte son.
    samples: VecDeque<f32>,
    /// Position du premier échantillon de `samples`.
    samples_pts: f64,
    volume: f32,
    muted: bool,

    /// Taille d'affichage en pixels physiques (0 = taille native).
    target_size: (u32, u32),
}

/// Résultat de l'attente du décodeur.
pub enum Room {
    Available,
    Full,
}

impl Playback {
    pub fn new(audio_clock: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                generation: 0,
                paused: false,
                anchor_pts: 0.0,
                anchor_at: Instant::now(),
                audio_clock,
                audio_started: false,
                video_started: false,
                frames: VecDeque::new(),
                last_shown: f64::NEG_INFINITY,
                eof_generation: None,
                samples: VecDeque::new(),
                samples_pts: 0.0,
                volume: 1.0,
                muted: false,
                target_size: (0, 0),
            }),
            wakeup: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- Horloge ---------------------------------------------------------

    pub fn now(&self) -> f64 {
        self.lock().now()
    }

    /// Repasse sur l'horloge système (pas de sortie audio).
    pub fn disable_audio_clock(&self) {
        self.lock().audio_clock = false;
    }

    pub fn is_paused(&self) -> bool {
        self.lock().paused
    }

    pub fn set_paused(&self, paused: bool) {
        let mut st = self.lock();
        let now = st.now();
        st.anchor_pts = now;
        st.anchor_at = Instant::now();
        st.paused = paused;
        drop(st);
        self.wakeup.notify_all();
    }

    /// Signale l'affichage de la première image après une ouverture ou un
    /// déplacement. Sans audio, l'horloge démarre à cet instant, sur cette
    /// image (sinon les images décodées pendant l'initialisation seraient
    /// considérées en retard et jetées).
    pub fn set_clock(&self, pts: f64) {
        let mut st = self.lock();
        st.video_started = true;
        if !st.audio_clock {
            st.anchor_pts = pts;
            st.anchor_at = Instant::now();
        }
    }

    /// Prépare un déplacement : vide les files et place l'horloge sur `target`.
    pub fn reset(&self, target: f64, generation: u64) {
        let mut st = self.lock();
        st.generation = generation;
        st.frames.clear();
        st.last_shown = f64::NEG_INFINITY;
        st.samples.clear();
        st.eof_generation = None;
        st.audio_started = false;
        st.video_started = false;
        st.anchor_pts = target;
        st.anchor_at = Instant::now();
        drop(st);
        self.wakeup.notify_all();
    }

    pub fn volume(&self) -> (f32, bool) {
        let st = self.lock();
        (st.volume, st.muted)
    }

    pub fn set_volume(&self, volume: f32, muted: bool) {
        let mut st = self.lock();
        st.volume = volume.clamp(0.0, 1.0);
        st.muted = muted;
    }

    // ---- Côté interface ----------------------------------------------------

    /// Taille à laquelle convertir les images (zone d'affichage à l'écran).
    pub fn set_target_size(&self, size: (u32, u32)) {
        self.lock().target_size = size;
    }

    pub fn target_size(&self) -> (u32, u32) {
        self.lock().target_size
    }

    /// Renvoie l'image à afficher maintenant (la plus récente dont l'heure
    /// est passée), ou la première disponible si `immediate`.
    pub fn take_frame(&self, immediate: bool) -> Option<VideoFrame> {
        let mut st = self.lock();
        let generation = st.generation;
        st.frames.retain(|f| f.generation == generation);
        let frame = if immediate {
            st.frames.pop_front()
        } else {
            let now = st.now();
            let mut due = None;
            while st.frames.front().is_some_and(|f| f.pts <= now) {
                due = st.frames.pop_front();
            }
            due
        };
        if let Some(f) = &frame {
            st.last_shown = f.pts;
        }
        drop(st);
        if frame.is_some() {
            self.wakeup.notify_all();
        }
        frame
    }

    /// Temps restant avant la prochaine image en file.
    pub fn time_to_next_frame(&self) -> Option<f64> {
        let st = self.lock();
        let now = st.now();
        st.frames.front().map(|f| (f.pts - now).max(0.0))
    }

    pub fn has_frames(&self) -> bool {
        !self.lock().frames.is_empty()
    }

    /// Vrai quand tout a été décodé et restitué.
    pub fn finished(&self) -> bool {
        let st = self.lock();
        st.eof_generation == Some(st.generation)
            && st.frames.is_empty()
            && (!st.audio_clock || st.samples.is_empty())
    }

    pub fn end_of_stream(&self) -> bool {
        let st = self.lock();
        st.eof_generation == Some(st.generation)
    }

    // ---- Côté décodeur -----------------------------------------------------

    /// Attend (au plus quelques ms) qu'il y ait de la place pour une image.
    /// Les images en retard, déjà dépassées par une suivante, sont jetées ici :
    /// la lecture continue même si l'interface ne se redessine pas (fenêtre
    /// réduite).
    pub fn wait_for_room(&self, max_frames: usize, max_audio: usize, video_done: bool) -> Room {
        let mut st = self.lock();
        let generation = st.generation;
        st.frames.retain(|f| f.generation == generation);
        if !st.paused {
            let now = st.now();
            while st.frames.len() >= 2 && st.frames[1].pts <= now {
                st.frames.pop_front();
            }
        }
        let full = |st: &State| {
            st.frames.len() >= max_frames
                || (st.samples.len() >= max_audio && (!st.frames.is_empty() || video_done))
        };
        if !full(&st) {
            return Room::Available;
        }
        let (st, _) = self
            .wakeup
            .wait_timeout(st, Duration::from_millis(20))
            .unwrap_or_else(|e| e.into_inner());
        if full(&st) { Room::Full } else { Room::Available }
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.lock().generation == generation
    }

    pub fn push_frame(&self, frame: VideoFrame) {
        let mut st = self.lock();
        if frame.generation != st.generation || frame.pts <= st.last_shown {
            return; // périmée, ou arrivée après une image plus récente
        }
        let index = st.frames.partition_point(|f| f.pts < frame.pts);
        st.frames.insert(index, frame);
    }

    pub fn push_audio(&self, generation: u64, pts: f64, samples: &[f32]) {
        let mut st = self.lock();
        if generation != st.generation || samples.is_empty() {
            return;
        }
        if st.samples.is_empty() {
            st.samples_pts = pts;
        }
        st.samples.extend(samples);
    }

    pub fn mark_end_of_stream(&self, generation: u64) {
        let mut st = self.lock();
        if generation == st.generation {
            st.eof_generation = Some(generation);
        }
    }

    // ---- Côté carte son ----------------------------------------------------

    /// Remplit le tampon de la carte son. `latency` = délai avant que le
    /// premier échantillon de `out` soit audible.
    pub fn fill_audio<T: cpal::SizedSample + cpal::FromSample<f32>>(
        &self,
        out: &mut [T],
        channels: usize,
        rate: f64,
        latency: f64,
    ) {
        let silence = T::from_sample(0.0f32);
        let mut st = self.lock();
        if st.paused || st.samples.is_empty() {
            out.fill(silence);
            return;
        }
        // Référence d'horloge : ce qui sera entendu dans `latency` secondes.
        st.anchor_pts = st.samples_pts - latency;
        st.anchor_at = Instant::now();
        st.audio_started = true;

        let gain = if st.muted { 0.0 } else { st.volume * st.volume };
        let n = (st.samples.len() / channels).min(out.len() / channels) * channels;
        for (dst, src) in out.iter_mut().zip(st.samples.drain(..n)) {
            *dst = T::from_sample(src * gain);
        }
        out[n..].fill(silence);
        st.samples_pts += (n / channels) as f64 / rate;
        drop(st);
        self.wakeup.notify_all();
    }
}

impl State {
    fn now(&self) -> f64 {
        if self.paused {
            return self.anchor_pts;
        }
        let elapsed = self.anchor_at.elapsed().as_secs_f64();
        let audio_running = self.audio_clock
            && !(self.eof_generation == Some(self.generation) && self.samples.is_empty());
        if audio_running {
            if !self.audio_started {
                // On attend le premier échantillon audible.
                return self.anchor_pts;
            }
            self.anchor_pts + elapsed.min(MAX_EXTRAPOLATION)
        } else if !self.video_started {
            // On attend la première image.
            self.anchor_pts
        } else {
            self.anchor_pts + elapsed
        }
    }
}
