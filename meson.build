project(
    'xdg-desktop-portal-shiny',
    'rust',
    version: run_command(
        'awk',
        '''
          {
            match($0, /\s*version\s*=\s*"(.*)"/, arr);
            if (arr[1] != "") {
              print arr[1];
              exit;
            }
          }
        ''',
        'Cargo.toml',
        check: true,
    ).stdout().strip(),
    meson_version: '>= 1.1.0',
)

cargo = find_program('cargo', version: '>= 1.90')
find_program('rustc', version: '>= 1.90')

prefix = get_option('prefix')
xdg_install_dir = prefix / get_option('libexecdir')
portal_dir = prefix / get_option('datadir') / 'xdg-desktop-portal' / 'portals'
dbus1_dir = prefix / get_option('datadir') / 'dbus-1' / 'services'
system_dir = prefix / get_option('libdir') / 'systemd' / 'user'

cargo_target_dir = meson.project_build_root() / 'cargo-target'
cargo_env = environment()
cargo_env.set('CARGO_TARGET_DIR', cargo_target_dir)

build_type = get_option('debug') ? 'debug' : 'release'
cargo_release_flag = get_option('debug') ? '' : '--release'

custom_target(
    'xdg-desktop-portal-shiny',
    output: 'xdg-desktop-portal-shiny',
    build_by_default: true,
    install: true,
    install_dir: xdg_install_dir,
    console: true,
    env: cargo_env,
    command: [
        cargo,
        'build',
        '--manifest-path', meson.project_source_root() / 'Cargo.toml',
        cargo_release_flag,
        '&&',
        'cp',
        cargo_target_dir / build_type / meson.project_name(),
        '@OUTPUT@',
    ],
)

global_conf = configuration_data()
global_conf.set('xdg_install_dir', xdg_install_dir)

configure_file(
    input: 'contrib/xdg-desktop-portal.shiny.service.in',
    output: 'xdg-desktop-portal-shiny.service',
    configuration: global_conf,
    install: true,
    install_dir: system_dir,
)

configure_file(
    input: 'contrib/org.freedesktop.impl.portal.desktop.shiny.service.in',
    output: 'org.freedesktop.impl.portal.desktop.shiny.service',
    configuration: global_conf,
    install: true,
    install_dir: dbus1_dir,
)

install_data('contrib/shiny.portal', install_dir: portal_dir)
