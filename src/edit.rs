//! Montage : passages supprimés de la vidéo, sélection courante et
//! historique d'annulation. Les positions sont en secondes depuis le début
//! de la vidéo.

/// Durée en dessous de laquelle un passage est ignoré.
const MIN_LENGTH: f64 = 1e-3;

/// Passages supprimés, triés et disjoints.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cuts {
    removed: Vec<(f64, f64)>,
}

impl Cuts {
    /// Supprime le passage `[a, b]` (bornes dans n'importe quel ordre),
    /// fusionné avec les passages déjà supprimés qu'il touche.
    pub fn remove(&mut self, a: f64, b: f64, duration: f64) {
        let (a, b) = (a.min(b).max(0.0), a.max(b).min(duration));
        if b - a < MIN_LENGTH {
            return;
        }
        let (mut start, mut end) = (a, b);
        self.removed.retain(|&(s, e)| {
            let touches = s <= end + MIN_LENGTH && e >= start - MIN_LENGTH;
            if touches {
                start = start.min(s);
                end = end.max(e);
            }
            !touches
        });
        let index = self.removed.partition_point(|&(s, _)| s < start);
        self.removed.insert(index, (start, end));
    }

    pub fn removed(&self) -> &[(f64, f64)] {
        &self.removed
    }

    pub fn is_empty(&self) -> bool {
        self.removed.is_empty()
    }

    /// Passages conservés, dans l'ordre.
    pub fn kept(&self, duration: f64) -> Vec<(f64, f64)> {
        let mut kept = Vec::new();
        let mut cursor = 0.0;
        for &(s, e) in &self.removed {
            if s - cursor >= MIN_LENGTH {
                kept.push((cursor, s));
            }
            cursor = e;
        }
        if duration - cursor >= MIN_LENGTH {
            kept.push((cursor, duration));
        }
        kept
    }

    pub fn kept_duration(&self, duration: f64) -> f64 {
        self.kept(duration).iter().map(|(s, e)| e - s).sum()
    }

    /// Passage supprimé contenant la position `t`, s'il y en a un.
    pub fn removed_at(&self, t: f64) -> Option<(f64, f64)> {
        self.removed.iter().copied().find(|&(s, e)| s <= t && t < e)
    }
}

/// État du montage d'une vidéo.
#[derive(Default)]
pub struct Editor {
    cuts: Cuts,
    history: Vec<Cuts>,
    /// Début et fin de la sélection (points d'entrée et de sortie).
    pub mark_in: Option<f64>,
    pub mark_out: Option<f64>,
}

impl Editor {
    pub fn cuts(&self) -> &Cuts {
        &self.cuts
    }

    /// Sélection complète, bornes ordonnées.
    pub fn selection(&self) -> Option<(f64, f64)> {
        match (self.mark_in, self.mark_out) {
            (Some(a), Some(b)) if (b - a).abs() >= MIN_LENGTH => Some((a.min(b), a.max(b))),
            _ => None,
        }
    }

    fn apply(&mut self, a: f64, b: f64, duration: f64) {
        let before = self.cuts.clone();
        self.cuts.remove(a, b, duration);
        if self.cuts != before {
            self.history.push(before);
        }
    }

    /// Supprime tout ce qui précède `position`.
    pub fn cut_before(&mut self, position: f64, duration: f64) {
        self.apply(0.0, position, duration);
    }

    /// Supprime tout ce qui suit `position`.
    pub fn cut_after(&mut self, position: f64, duration: f64) {
        self.apply(position, duration, duration);
    }

    /// Supprime la sélection, puis l'efface. Renvoie `false` sans sélection.
    pub fn cut_selection(&mut self, duration: f64) -> bool {
        let Some((a, b)) = self.selection() else { return false };
        self.apply(a, b, duration);
        self.mark_in = None;
        self.mark_out = None;
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn undo(&mut self) {
        if let Some(previous) = self.history.pop() {
            self.cuts = previous;
        }
    }

    /// Rétablit toute la vidéo (annulable).
    pub fn reset(&mut self) {
        if !self.cuts.is_empty() {
            self.history.push(std::mem::take(&mut self.cuts));
        }
        self.mark_in = None;
        self.mark_out = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: f64 = 100.0;

    #[test]
    fn rien_de_supprime() {
        let cuts = Cuts::default();
        assert_eq!(cuts.kept(D), vec![(0.0, D)]);
        assert_eq!(cuts.kept_duration(D), D);
        assert_eq!(cuts.removed_at(50.0), None);
    }

    #[test]
    fn debut_fin_et_milieu() {
        let mut cuts = Cuts::default();
        cuts.remove(0.0, 10.0, D);
        cuts.remove(90.0, D, D);
        cuts.remove(40.0, 50.0, D);
        assert_eq!(cuts.kept(D), vec![(10.0, 40.0), (50.0, 90.0)]);
        assert_eq!(cuts.kept_duration(D), 70.0);
        assert_eq!(cuts.removed_at(45.0), Some((40.0, 50.0)));
        assert_eq!(cuts.removed_at(50.0), None);
    }

    #[test]
    fn fusion_et_bornes() {
        let mut cuts = Cuts::default();
        cuts.remove(30.0, 20.0, D); // bornes inversées
        cuts.remove(25.0, 40.0, D); // chevauchement
        cuts.remove(40.0, 45.0, D); // contigu
        cuts.remove(-5.0, 1.0, D); // borné à 0
        cuts.remove(99.0, 150.0, D); // borné à la durée
        assert_eq!(cuts.removed(), &[(0.0, 1.0), (20.0, 45.0), (99.0, D)]);
        cuts.remove(10.0, 10.0, D); // vide : ignoré
        assert_eq!(cuts.removed().len(), 3);
    }

    #[test]
    fn tout_supprime() {
        let mut cuts = Cuts::default();
        cuts.remove(0.0, D, D);
        assert!(cuts.kept(D).is_empty());
    }

    #[test]
    fn selection_annulation_et_retablissement() {
        let mut editor = Editor::default();
        assert!(!editor.cut_selection(D));
        editor.mark_in = Some(60.0);
        editor.mark_out = Some(30.0);
        assert_eq!(editor.selection(), Some((30.0, 60.0)));
        assert!(editor.cut_selection(D));
        assert_eq!(editor.selection(), None);
        editor.cut_before(10.0, D);
        assert_eq!(editor.cuts().removed(), &[(0.0, 10.0), (30.0, 60.0)]);

        editor.undo();
        assert_eq!(editor.cuts().removed(), &[(30.0, 60.0)]);
        editor.reset();
        assert!(editor.cuts().is_empty());
        editor.undo();
        assert_eq!(editor.cuts().removed(), &[(30.0, 60.0)]);
        editor.undo();
        assert!(editor.cuts().is_empty());
        assert!(!editor.can_undo());
    }
}
