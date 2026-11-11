{
  lib,
  rustPlatform,
  makeWrapper,
  ffmpeg-headless,
  chromaprint,
  yt-dlp,
  sqlite,
}:

let
  cargo = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargo.package.name;
  inherit (cargo.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../static
      ../tests/cli.rs
      ../tests/fixtures
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];
  # libsqlite3-sys still asks the linker for -lsqlite3 under the nixpkgs cargo hooks.
  buildInputs = [ sqlite ];
  # The audio, tagging and pipeline tests use the real tools; without them they skip.
  nativeCheckInputs = [
    ffmpeg-headless
    chromaprint
  ];

  # yt-dlp, ffmpeg/ffprobe and fpcalc are subprocesses; *_PATH settings still override.
  postInstall = ''
    wrapProgram $out/bin/navidrome-proxy \
      --prefix PATH : ${
        lib.makeBinPath [
          yt-dlp
          ffmpeg-headless
          chromaprint
        ]
      }
  '';

  meta = {
    description = cargo.package.description;
    homepage = "https://github.com/daz9e/navidrome-proxy";
    license = lib.licenses.mit;
    mainProgram = "navidrome-proxy";
    platforms = lib.platforms.linux;
  };
}
