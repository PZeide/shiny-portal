{
  lib,
  rustPlatform,
  pkg-config,
  xdg-desktop-portal,
  libgbm,
  libGL,
  wayland,
  pipewire,
  ...
}:
rustPlatform.buildRustPackage {
  pname = "xdg-desktop-portal-shiny";
  version = (fromTOML (builtins.readFile ./Cargo.toml)).package.version;
  src = ./.;

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ];

  cargoLock = {
    lockFile = ./Cargo.lock;
  };

  buildInputs = [
    xdg-desktop-portal
    libgbm
    libGL
    wayland
    pipewire
  ];

  postPatch = ''
    substituteInPlace contrib/org.freedesktop.impl.portal.desktop.shiny.service.in \
      --replace-fail @libexecdir@ "$out/libexec"
    substituteInPlace contrib/xdg-desktop-portal.shiny.service.in \
      --replace-fail @libexecdir@ "$out/libexec"
  '';

  postInstall = ''
    install -Dm755 $out/bin/xdg-desktop-portal-shiny \
      $out/libexec/xdg-desktop-portal-shiny
    install -Dm644 contrib/shiny.portal \
      $out/share/xdg-desktop-portal/portals/shiny.portal
    install -Dm644 contrib/org.freedesktop.impl.portal.desktop.shiny.service.in \
      $out/share/dbus-1/services/org.freedesktop.impl.portal.desktop.shiny.service
    install -Dm644 contrib/xdg-desktop-portal.shiny.service.in \
      $out/lib/systemd/user/xdg-desktop-portal-shiny.service
  '';

  meta = {
    description = "Shiny Portal | @PZeide";
    homepage = "https://github.com/PZeide/shiny-portal";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
