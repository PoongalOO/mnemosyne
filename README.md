# Mnemosyne

Lecteur vidéo minimaliste en Rust, basé sur **FFmpeg** (`ffmpeg-next`) pour le
décodage, **egui/eframe** pour l'interface et **cpal** pour le son.

- Conteneurs : MP4, WebM, AVI, MKV
- Codecs vidéo : H.264/AVC et H.265/HEVC, 8 et 10 bits (les autres codecs
  reconnus par FFmpeg sont lus aussi, avec un avertissement)
- Son : toute piste audio lisible par FFmpeg (AAC, Opus, MP3, AC-3…), avec
  synchronisation audio/vidéo
- Décodage matériel : VA-API (Intel/AMD), NVDEC/CUDA (NVIDIA), VDPAU (pilote
  NVIDIA propriétaire uniquement) sous Linux ;
  D3D11VA/DXVA2 sous Windows ; VideoToolbox sous macOS. Repli automatique sur
  le décodage logiciel si le GPU ne gère pas le flux (ex. H.264 10 bits)
- Lecture / pause, stop, barre de progression avec déplacement précis, volume,
  image par image, plein écran, glisser-déposer, raccourcis clavier
- Interface en thème sombre, clair ou celui du système, police Inter et
  icônes vectorielles (Phosphor) ; écran d'accueil avec les fichiers récents ;
  contrôles flottants masqués automatiquement en plein écran ; messages en
  surimpression (volume, déplacement, coupes…)
- Frise avec graduations, zoom à la molette, miniature au survol, repères des
  images clés et forme d'onde du son (outils de montage affichés)
- Réglages mémorisés d'une session à l'autre : volume, thème, accélération
  matérielle, outils affichés, mode d'enregistrement, taille et position de
  la fenêtre
- Montage : couper le début, la fin ou un passage entre deux points, puis
  enregistrer le résultat dans un **nouveau fichier** (l'original n'est jamais
  modifié)

## Installation

Les paquets sont publiés sur la page
[Releases](https://github.com/PoongalOO/mnemosyne/releases) :

- **Debian 13 / Ubuntu 24.04** : `sudo apt install ./mnemosyne_*.deb` (le
  paquet installe aussi l'entrée du menu des applications et l'icône) ;
- **Windows** : décompresser l'archive `.zip` et lancer `mnemosyne.exe` (le
  programme n'est pas signé : Windows SmartScreen peut afficher un
  avertissement au premier lancement) ;
- **NixOS** : `nix run github:PoongalOO/mnemosyne`, ou en ajoutant le flake
  à la configuration (`inputs.mnemosyne.url = "github:PoongalOO/mnemosyne";`
  puis `mnemosyne.packages.${system}.default` ou `overlays.default`).

Pour compiler soi-même, voir ci-dessous.

## Dépendances système (Debian / Ubuntu)

```sh
sudo apt install pkg-config clang libasound2-dev \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev
```

(`clang` fournit la libclang utilisée par `bindgen` pour générer les liaisons
FFmpeg ; `libasound2-dev` est requis par `cpal`.) Pour le décodage matériel, il
faut aussi le pilote VA-API du GPU (`intel-media-va-driver`,
`mesa-va-drivers`…) ; `vainfo` liste les profils pris en charge.

## Compilation et lancement

```sh
cargo run --release -- chemin/vers/video.mkv
cargo run --release -- --no-hw chemin/vers/video.mkv   # décodage logiciel
```

Sans argument, l'écran d'accueil propose le bouton **Ouvrir une vidéo…** et
les derniers fichiers ouverts ; on peut aussi glisser un fichier dans la
fenêtre. Le menu **⚙ Réglages** contient l'accélération matérielle (bascule
en cours de lecture), le thème et l'aide ; le bouton **ⓘ** affiche les
caractéristiques de la vidéo, dont le mode de décodage réellement utilisé,
et le bouton **@** présente l'application, sa licence et son auteur.
`--no-hw` désactive le décodage matériel pour la session sans modifier le
réglage mémorisé.

| Touche / geste              | Action                                   |
|-----------------------------|------------------------------------------|
| Espace, clic sur l'image    | Lecture / pause                          |
| ← / →                       | Reculer / avancer de 5 s                 |
| , / .                       | Image précédente / suivante              |
| ↑ / ↓                       | Volume                                   |
| M                           | Couper / rétablir le son                 |
| F, double-clic sur l'image  | Plein écran                              |
| Échap                       | Quitter le plein écran                   |
| I / O                       | Début / fin de la sélection              |
| Maj+I / Maj+O               | Aller au début / à la fin de la sélection|
| Suppr                       | Supprimer la sélection                   |
| Ctrl+Z                      | Annuler la dernière coupe                |
| Ctrl+O                      | Ouvrir une vidéo                         |
| F1, ?                       | Aide (liste des raccourcis)              |
| Molette sur la frise        | Zoom (défilement horizontal : déplacer)  |
| Maj + glisser sur la frise  | Créer une sélection                      |
| Glisser un repère jaune     | Déplacer le début / la fin               |
| Clic droit sur la frise     | Repères, rétablir un passage…            |

En plein écran, les contrôles flottent au bas de l'image et disparaissent
(avec le pointeur) après 2,5 s sans mouvement de souris pendant la lecture ;
ils restent affichés en pause.

## Montage

Le bouton **✂ Montage** affiche ou masque la barre d'outils de montage (elle
s'ouvre aussi avec les touches I / O). Elle agit à la position de lecture :

- **Couper avant** / **Couper après** : supprime tout ce qui précède / suit ;
- **Début [I]** / **Fin [O]** puis **Supprimer la sélection** : supprime le
  passage entre deux points ;
- **Annuler** / **Tout rétablir**.

Les passages supprimés apparaissent en rouge sur la frise, la sélection en
jaune (ses repères se déplacent à la souris) ; une légende le rappelle. Le
clic droit sur un passage supprimé permet de le rétablir. Avec **Aperçu**
coché, la lecture saute les passages supprimés pour vérifier le résultat.

Le bouton **Passages** ouvre un panneau listant les passages conservés et
supprimés (aller au début, supprimer, rétablir) et permet de couper ou de
sélectionner un passage en saisissant ses temps (`83.4`, `1:23.4`,
`1:02:03`).

**Enregistrer…** propose un nom à côté de l'original (`<nom>_montage.<ext>`,
numéroté s'il existe déjà) et refuse le fichier d'origine comme destination.
L'écriture se fait dans un fichier `.part` renommé à la fin : une erreur ou
une annulation ne laisse aucun fichier partiel. La fenêtre affiche le temps
restant estimé, signale la fin dans la barre des tâches si elle n'est pas au
premier plan et propose d'ouvrir la vidéo obtenue ou son dossier. Deux modes
(le dernier choisi est mémorisé) :

- **Précis (réencodage)** : coupes à l'image près. La vidéo est réencodée dans
  son codec d'origine (libx264 CRF 18 / libx265 CRF 20, preset *fast*), le son
  en AAC (MP3 dans un AVI). Durée : proche du temps réel en 1080p, plusieurs
  fois plus en 4K HEVC.
- **Rapide (sans réencodage)** : copie des données, quasi instantanée et sans
  perte, mais chaque coupe est calée sur une image clé (début reculé, fin
  avancée). La fenêtre affiche la durée réellement obtenue ; si la vidéo n'a
  pas d'image clé aux endroits coupés (fréquent pour les fichiers réencodés
  avec de longs GOP), elle le signale et le mode précis est nécessaire.

Formats de sortie : MP4, MKV ou AVI (une source WebM est enregistrée en MKV,
le WebM n'admettant pas H.264/H.265).

## Architecture

- `src/decoder.rs` : thread de démultiplexage et de décodage (vidéo + audio).
  Les images décodées sont confiées à un petit pool de threads qui les
  rapatrie du GPU si besoin et les convertit en RGBA (swscale, matrice
  BT.601/709/2020 selon la vidéo) **à la taille d'affichage** : une vidéo 4K
  dans une fenêtre 720p n'est convertie et transférée qu'en 720p. L'audio est
  rééchantillonné (swresample) au format de la carte son.
- `src/hwaccel.rs` : configuration du décodage matériel (`hw_device_ctx`,
  callback `get_format`) et transfert des images GPU → mémoire centrale.
- `src/playback.rs` : état partagé — file d'images triée, tampon audio et
  horloge maître. Quand il y a du son, c'est la carte son qui fait avancer
  l'horloge et la vidéo s'y cale ; sinon l'horloge suit le temps système.
  Chaque déplacement porte un numéro de « génération » pour écarter les
  données périmées.
- `src/audio.rs` : sortie audio cpal (f32/i16/u16/i32 selon le périphérique),
  avec prise en compte de la latence de la carte son.
- `src/edit.rs` : modèle de montage (passages supprimés, sélection,
  historique d'annulation).
- `src/export.rs` : enregistrement de la vidéo montée dans un thread
  (progression, annulation) ; réencodage ou copie avec calage sur les images
  clés (y compris GOP ouverts HEVC).
- `src/main.rs` : interface egui (écran d'accueil, lecteur, barre d'outils,
  panneau des passages, plein écran, fenêtres d'enregistrement et d'aide).
- `src/timeline.rs` : frise (zoom, graduations, images clés, forme d'onde,
  repères déplaçables, menu contextuel).
- `src/thumbnails.rs` : miniatures de survol, produites par un second
  décodeur logiciel dans son propre thread (seule la dernière demande compte).
- `src/waveform.rs` : forme d'onde (crête par tranche de 20 ms), calculée en
  arrière-plan.
- `src/settings.rs` : réglages mémorisés (persistance d'eframe, dans
  `~/.local/share/mnemosyne/` sous Linux).
- `src/theme.rs` : thèmes sombre et clair, couleurs, icônes, icône de la
  fenêtre (dessinée par le programme). Les polices sont intégrées à
  l'exécutable : [Inter](https://rsms.me/inter/) (`assets/Inter-Regular.ttf`,
  licence SIL OFL 1.1) et les icônes [Phosphor](https://phosphoricons.com/)
  (`assets/Phosphor.ttf`, licence MIT, réduite aux points de code des
  icônes ; voir `assets/LICENSE-*.txt`).

Le projet utilise l'édition Rust 2024 et suit la version stable courante de
Rust (installée avec [rustup](https://rustup.rs) : `rustup update stable`).
`ffmpeg-next` détecte à la compilation la version de FFmpeg installée
(compatible avec FFmpeg 3.4 et suivants).

## Intégration continue et paquets

- `.github/workflows/ci.yml` : clippy (sans avertissement) et tests à chaque
  envoi sur `main` et à chaque pull request.
- `.github/workflows/release.yml` : à chaque tag `vX.Y.Z` (qui doit
  correspondre à la version de `Cargo.toml`), fabrication des paquets puis
  publication d'une release GitHub :
  - `.deb` pour Debian 13 et Ubuntu 24.04 avec
    [cargo-deb](https://github.com/kornelski/cargo-deb), compilés dans un
    conteneur de chaque distribution (chacune a sa version de FFmpeg) ;
  - archive Windows : exécutable et DLL FFmpeg (build GPL de
    [BtbN](https://github.com/BtbN/FFmpeg-Builds)) ;
  - vérification du flake Nix (`flake.nix`, `nix/package.nix`).

  Lancé à la main depuis l'onglet *Actions*, il fabrique les paquets sans
  publier de release.

Pour publier une version : changer `version` dans `Cargo.toml`, commiter,
puis `git tag v0.2.0 && git push origin v0.2.0`.

Fichiers de paquet communs : `packaging/mnemosyne.desktop` et l'icône
(`packaging/mnemosyne.svg`, `.png`).

## Limites

- Les images décodées par le GPU sont rapatriées en mémoire centrale puis
  converties sur le CPU (pas d'interopérabilité GPU directe type dmabuf/EGL).
  Sur un petit portable, une 4K affichée en plein écran 4K peut donc ne pas
  tenir le temps réel ; dans une fenêtre de taille courante, le décodage
  matériel divise par deux la charge CPU par rapport au logiciel.
- Seule la première piste audio (la « meilleure » selon FFmpeg) est lue et
  enregistrée ; pas de sous-titres. Les métadonnées (titre, date…) ne sont
  pas recopiées dans la vidéo montée.
- Mode rapide avec des GOP ouverts (HEVC encodé par x265) : les quelques
  images précédant l'image clé de fin d'un passage (≈ 0,1 s) sont omises,
  car elles dépendent d'images retirées.

## Licence

Mnémosyne est un logiciel libre distribué sous licence GNU GPL, version 3
ou ultérieure (voir `LICENSE`), sans aucune garantie.

Auteur : PoongalOO <sam@poongaloo.org>
