//! Réglages conservés d'une session à l'autre (via la persistance d'eframe,
//! qui mémorise aussi la taille et la position de la fenêtre).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Nombre de fichiers récents mémorisés.
const MAX_RECENT: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub volume: f32,
    pub muted: bool,
    pub hardware: bool,
    pub preview_cuts: bool,
    pub show_montage: bool,
    /// Panneau de la liste des passages.
    pub show_segments: bool,
    pub theme: ThemeChoice,
    /// Fichiers ouverts récemment, le plus récent en premier.
    pub recent: Vec<PathBuf>,
    /// Dernier mode d'enregistrement choisi : rapide (sinon précis).
    pub fast_export: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
            hardware: true,
            preview_cuts: true,
            show_montage: false,
            show_segments: false,
            theme: ThemeChoice::Dark,
            recent: Vec::new(),
            fast_export: false,
        }
    }
}

impl Settings {
    pub fn add_recent(&mut self, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        self.recent.retain(|p| *p != path);
        self.recent.insert(0, path);
        self.recent.truncate(MAX_RECENT);
    }

    pub fn remove_recent(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
    }
}
