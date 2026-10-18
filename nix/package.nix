{
  lib,
  rustPlatform,
  pkg-config,
  makeWrapper,
  ffmpeg,
  alsa-lib,
  libGL,
  libxkbcommon,
  wayland,
  libx11,
  libxcursor,
  libxi,
  libxrandr,
}:

let
  manifest = (lib.importTOML ../Cargo.toml).package;
  # Bibliothèques chargées dynamiquement par eframe/winit à l'exécution.
  runtimeLibs = [ libGL libxkbcommon wayland libx11 libxcursor libxi libxrandr ];
in
rustPlatform.buildRustPackage {
  pname = manifest.name;
  inherit (manifest) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../assets
      ../packaging
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config makeWrapper rustPlatform.bindgenHook ];
  buildInputs = [ ffmpeg alsa-lib ];

  postInstall = ''
    install -Dm644 packaging/mnemosyne.desktop -t $out/share/applications
    install -Dm644 packaging/mnemosyne.svg -t $out/share/icons/hicolor/scalable/apps
    install -Dm644 packaging/mnemosyne.png -t $out/share/icons/hicolor/256x256/apps
  '';

  postFixup = ''
    wrapProgram $out/bin/mnemosyne --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs}
  '';

  passthru = { inherit runtimeLibs; };

  meta = {
    description = manifest.description;
    homepage = manifest.repository;
    license = lib.licenses.gpl3Plus;
    maintainers = [ ];
    mainProgram = "mnemosyne";
    platforms = lib.platforms.linux;
  };
}
