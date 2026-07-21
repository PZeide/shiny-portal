use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

const DEFAULT_CONFIG: &str = r#"# Maximum screencast capture rate. 0 disables the limit.
max_fps = 0

# Allow PipeWire to fall back to shared-memory buffers when DMA-BUF is unavailable when screencasting.
allow_screencast_shm = false
"#;

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub max_fps: u32,
    pub allow_screencast_shm: bool,
}

impl Config {
    pub fn load_or_create() -> anyhow::Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("configuration path has no parent"))?;
            fs::create_dir_all(parent)?;
            fs::write(&path, DEFAULT_CONFIG)?;
        }

        let source = fs::read_to_string(&path)?;
        let config = toml::from_str(&source)
            .map_err(|err| anyhow::anyhow!("invalid configuration {}: {err}", path.display()))?;
        Ok(config)
    }
}

fn config_path() -> anyhow::Result<PathBuf> {
    if let Some(config_home) = non_empty_env("XDG_CONFIG_HOME") {
        return Ok(Path::new(&config_home)
            .join("shiny")
            .join("portal-config.toml"));
    }

    let home = non_empty_env("HOME")
        .ok_or_else(|| anyhow::anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(Path::new(&home)
        .join(".config")
        .join("xdg-desktop-portal-shiny")
        .join("config.toml"))
}

fn non_empty_env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}
