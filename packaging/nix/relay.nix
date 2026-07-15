{
  lib,
  rustPlatform,
}:

let
  manifest = builtins.fromTOML (builtins.readFile ../../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = "breakd-relay";
  inherit (manifest.package) version;

  src = ../..;
  cargoLock.lockFile = ../../Cargo.lock;

  cargoBuildFlags = [
    "-p"
    "breakd-relay"
  ];
  cargoTestFlags = [
    "-p"
    "breakd-relay"
  ];

  meta = {
    description = "Small authenticated WebSocket relay for breakd co-op rooms";
    homepage = "https://github.com/simonwinther/breakd";
    license = lib.licenses.mit;
    mainProgram = "breakd-relay";
    platforms = lib.platforms.linux;
  };
}
