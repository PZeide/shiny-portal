project('xdg-desktop-portal-shiny', 'rust', meson_version: '>= 1.1.0')

cargo = find_program('cargo')
prefix = get_option('prefix')
libexecdir = prefix / get_option('libexecdir')

custom_target(
  'xdg-desktop-portal-shiny',
  output: 'xdg-desktop-portal-shiny',
  build_by_default: true,
  install: true,
  install_dir: libexecdir,
  command: [
    'sh',
    '-c',
    '@0@ build --manifest-path "$1/Cargo.toml" --target-dir "$2" --release && cp "$2/release/xdg-desktop-portal-shiny" "$3"'.format(cargo.full_path()),
    'sh',
    meson.project_source_root(),
    meson.project_build_root() / 'target',
    '@OUTPUT@',
  ],
)
