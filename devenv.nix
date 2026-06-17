{pkgs, ...}: {
  env.LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

  # https://devenv.sh/languages/
  languages.rust = {
    enable = true;
    wild.enable = true;
  };

  # https://devenv.sh/packages/
  packages = with pkgs; [
    llvmPackages.clang

    libgbm
    libGL.dev
    wayland.dev
    pipewire.dev
  ];

  # See full reference at https://devenv.sh/reference/options/
}
