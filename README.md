# Mnemosyne

Lecteur vidéo minimaliste en Rust, basé sur **FFmpeg** (`ffmpeg-next`) pour le
décodage, **egui/eframe** pour l'interface et **cpal** pour le son.

- Conteneurs : MP4, WebM, AVI, MKV
- Codecs vidéo : H.264/AVC et H.265/HEVC, 8 et 10 bits (les autres codecs
  reconnus par FFmpeg sont lus aussi, avec un avertissement)
- Son : toute piste audio lisible par FFmpeg (AAC, Opus, MP3, AC-3…), avec
  synchronisation audio/vidéo
- Décodage matériel : VA-API (Intel/AMD), NVDEC/CUDA (NVIDIA), VDPAU sous Linux ;
  D3D11VA/DXVA2 sous Windows ; VideoToolbox sous macOS. Repli automatique sur
  le décodage logiciel si le GPU ne gère pas le flux (ex. H.264 10 bits)
- Lecture / pause, stop, barre de progression avec déplacement précis, volume,
  image par image, glisser-déposer, raccourcis clavier
- Montage : couper le début, la fin ou un passage entre deux points, puis
  enregistrer le résultat dans un **nouveau fichier** (l'original n'est jamais
  modifié)

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

Sans argument, utilisez le bouton **Ouvrir** ou glissez un fichier dans la
fenêtre. La case **Accélération matérielle** bascule le mode de décodage en
cours de lecture ; la ligne d'information indique le mode réellement utilisé.

| Touche  | Action                          |
|---------|---------------------------------|
| Espace  | Lecture / pause                 |
| ← / →   | Reculer / avancer de 5 s        |
| , / .   | Image précédente / suivante     |
| ↑ / ↓   | Volume                          |
| M       | Couper / rétablir le son        |
| I / O   | Début / fin de la sélection     |
| Suppr   | Supprimer la sélection          |
| Ctrl+Z  | Annuler la dernière coupe       |

## Montage

La barre **Montage** agit à la position de lecture :

- **Couper avant** / **Couper après** : supprime tout ce qui précède / suit ;
- **Début [I]** / **Fin [O]** puis **Supprimer la sélection** : supprime le
  passage entre deux points ;
- **Annuler** / **Tout rétablir**.

Les passages supprimés apparaissent en rouge sur la frise, la sélection en
jaune. Avec **Aperçu** coché, la lecture saute les passages supprimés pour
vérifier le résultat.

**Enregistrer…** propose un nom à côté de l'original (`<nom>_montage.<ext>`,
numéroté s'il existe déjà) et refuse le fichier d'origine comme destination.
L'écriture se fait dans un fichier `.part` renommé à la fin : une erreur ou
une annulation ne laisse aucun fichier partiel. Deux modes :

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
- `src/main.rs` : interface egui (lecteur, frise de montage, fenêtre
  d'enregistrement).

Le projet utilise l'édition Rust 2024 et suit la version stable courante de
Rust (installée avec [rustup](https://rustup.rs) : `rustup update stable`).
`ffmpeg-next` détecte à la compilation la version de FFmpeg installée
(compatible avec FFmpeg 3.4 et suivants).

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
