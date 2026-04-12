{
  lib,
  stdenv,
  rustPlatform,
  cargo,
  rustc,
  meson,
  ninja,
  pkg-config,
  xdg-desktop-portal,
  libgbm,
  libGL,
  wayland,
  pipewire,
  ...
}:
stdenv.mkDerivation {
  pname = "xdg-desktop-portal-shiny";
  version = (fromTOML (builtins.readFile ./Cargo.toml)).package.version;
  src = ./.;

  nativeBuildInputs = [
    pkg-config
    meson
    ninja
    rustc
    cargo
    rustPlatform.cargoSetupHook
    rustPlatform.bindgenHook
  ];

  cargoDeps = rustPlatform.importCargoLock {
    lockFile = ./Cargo.lock;
  };

  buildInputs = [
    xdg-desktop-portal
    libgbm
    libGL
    wayland
    pipewire
  ];

  postInstall = ''
    patchelf \
      --add-needed libwayland-client.so.0 \
      --add-rpath ${lib.makeLibraryPath [wayland]} \
      $out/libexec/xdg-desktop-portal-shiny
  '';

  meta = {
    description = "Shiny Portal | @PZeide";
    homepage = "https://github.com/PZeide/shiny-portal";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
