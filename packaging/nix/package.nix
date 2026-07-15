{
  lib,
  rustPlatform,
  pkg-config,
  wrapGAppsHook4,
  gtk4,
  gtk4-layer-shell,
  libcanberra,
  wayland,
}:

let
  manifest = builtins.fromTOML (builtins.readFile ../../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = "breakd";
  inherit (manifest.package) version;

  src = ../..;
  cargoLock.lockFile = ../../Cargo.lock;

  strictDeps = true;
  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
  ];
  buildInputs = [
    gtk4
    gtk4-layer-shell
    libcanberra
    wayland
  ];

  cargoBuildFlags = [
    "-p"
    "breakd"
  ];
  cargoTestFlags = [ "--workspace" ];

  postInstall = ''
    install -Dm644 crates/platform-linux/assets/*.oga -t "$out/share/breakd"
    install -Dm644 packaging/io.github.simonwinther.breakd.settings.desktop \
      "$out/share/applications/io.github.simonwinther.breakd.settings.desktop"
    install -Dm644 config.example.toml "$out/share/doc/breakd/config.example.toml"
    install -Dm644 README.md "$out/share/doc/breakd/README.md"
    install -Dm644 LICENSE "$out/share/licenses/breakd/LICENSE"
    install -Dm644 THIRD_PARTY_NOTICES.md \
      "$out/share/licenses/breakd/THIRD_PARTY_NOTICES.md"

    install -Dm644 packaging/systemd/breakd.service \
      "$out/lib/systemd/user/breakd.service"
    substituteInPlace "$out/lib/systemd/user/breakd.service" \
      --replace-fail /usr/bin/breakd "$out/bin/breakd"
  '';

  meta = {
    description = "Wayland-native break reminder with multi-monitor overlays";
    homepage = "https://github.com/simonwinther/breakd";
    license = [
      lib.licenses.mit
      lib.licenses.bsd2
    ];
    mainProgram = "breakd";
    platforms = lib.platforms.linux;
  };
}
