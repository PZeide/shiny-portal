use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::mpsc,
};
use tracing::{debug, warn};

pub static SHINY_SHELL_COMMAND: &str = "shiny-shell";

#[derive(Error, Debug)]
pub enum ShellError {
    #[error("process error: {0}")]
    Io(#[from] std::io::Error),
    #[error("shiny-shell error: {0}")]
    ShinyShell(String),
    #[error("ipc error: {0}")]
    Ipc(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "status", rename_all = "camelCase")]
enum ShellCallResult<T> {
    #[serde(rename_all = "camelCase")]
    Ok { data: T },
    #[serde(rename_all = "camelCase")]
    Error { message: String },
}

#[derive(Clone, Default)]
pub struct ShinyShell;

impl ShinyShell {
    async fn call<T>(
        &self,
        target: &str,
        method_name: &str,
        parameters: &[&str],
    ) -> Result<T, ShellError>
    where
        T: serde::de::DeserializeOwned,
    {
        debug!("calling shell method: {target}::{method_name}");

        let output = Command::new(SHINY_SHELL_COMMAND)
            .arg("ipc")
            .arg("call")
            .arg(target)
            .arg(method_name)
            .args(parameters)
            .output()
            .await?;

        if !output.status.success() || !output.stderr.is_empty() {
            let error = String::from_utf8_lossy(&output.stderr)
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("process failed or returned non-zero exit code")
                .to_string();

            return Err(ShellError::ShinyShell(error));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stdout = stdout.trim();

        match serde_json::from_str::<ShellCallResult<serde_json::Value>>(stdout) {
            Ok(ShellCallResult::Ok { data }) => Ok(serde_json::from_value::<T>(data)?),
            Ok(ShellCallResult::Error { message }) => Err(ShellError::Ipc(message)),
            Err(err) => Err(ShellError::ShinyShell(if stdout.is_empty() {
                err.to_string()
            } else {
                stdout.to_string()
            })),
        }
    }

    async fn listen<T>(&self, target: &str, signal: &str) -> Result<mpsc::Receiver<T>, ShellError>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        debug!("listening for shell signal: {target}::{signal}");

        let mut child = Command::new(SHINY_SHELL_COMMAND)
            .arg("ipc")
            .arg("listen")
            .arg(target)
            .arg(signal)
            .stdout(std::process::Stdio::piped())
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ShellError::Ipc("failed to open stdout pipe".into()))?;

        let (tx, rx) = mpsc::channel::<T>(32);

        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    line_result = reader.next_line() => {
                        match line_result {
                            Ok(Some(line)) => {
                                let line = line.trim();
                                if line.is_empty() {
                                    continue;
                                }

                                match serde_json::from_str::<T>(line) {
                                    Ok(data) => {
                                        let _ = tx.send(data).await;
                                    }
                                    Err(err) => warn!("error deserializing shell signal: {err}"),
                                }
                            }
                            Ok(None) => break,
                            Err(err) => {
                                warn!("error reading shell signal: {err}");
                                break;
                            }
                        }
                    }
                    _ = tx.closed() => {
                        let _ = child.kill().await;
                        break;
                    }
                }
            }
        });

        Ok(rx)
    }

    pub async fn region_selector(
        &self,
        options: RegionSelectorOptions,
    ) -> Result<RegionSelectorResult, ShellError> {
        let json = serde_json::to_string(&options)?;
        let request_id = self
            .call::<RegionSelectorRequestResult>("region-selector", "request", &[&json])
            .await?
            .0;

        let mut rx = self
            .listen::<RegionSelectorResult>("region-selector", "result")
            .await?;

        while let Some(result) = rx.recv().await {
            match &result {
                RegionSelectorResult::Selected { key, .. }
                | RegionSelectorResult::Cancelled { key }
                    if key == &request_id =>
                {
                    return Ok(result);
                }
                _ => {}
            }
        }

        Err(ShellError::Ipc(
            "stream closed without picker result".into(),
        ))
    }

    pub async fn share_picker(
        &self,
        options: SharePickerOptions,
    ) -> Result<SharePickerResult, ShellError> {
        let json = serde_json::to_string(&options)?;
        let request_id = self
            .call::<SharePickerRequestResult>("share-picker", "request", &[&json])
            .await?
            .0;

        let mut rx = self
            .listen::<SharePickerResult>("share-picker", "result")
            .await?;

        while let Some(result) = rx.recv().await {
            match &result {
                SharePickerResult::Selected { key, .. } | SharePickerResult::Cancelled { key }
                    if key == &request_id =>
                {
                    return Ok(result);
                }
                _ => {}
            }
        }

        Err(ShellError::Ipc(
            "stream closed without picker result".into(),
        ))
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CustomRegion {
    pub monitor: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Deserialize, Debug)]
struct RegionSelectorRequestResult(String);

#[derive(Deserialize, Debug)]
struct SharePickerRequestResult(String);

#[derive(Deserialize, Debug)]
#[serde(tag = "status", rename_all = "camelCase")]
#[allow(dead_code)]
pub enum RegionSelectorResult {
    Selected { key: String, result: CustomRegion },
    Cancelled { key: String },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "status", rename_all = "camelCase")]
#[allow(dead_code)]
pub enum SharePickerResult {
    Selected {
        key: String,
        result: SelectionResult,
    },
    Cancelled {
        key: String,
    },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
#[allow(dead_code)]
pub enum SelectionResult {
    #[serde(rename_all = "camelCase")]
    Monitor {
        allow_restore_token: bool,
        monitor: String,
    },
    #[serde(rename_all = "camelCase")]
    Window {
        allow_restore_token: bool,
        window_address: String,
        stable_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Custom {
        allow_restore_token: bool,
        region: CustomRegion,
    },
}

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct RegionSelectorOptions {
    pub freeze: Option<bool>,
    pub hint_windows: Option<bool>,
    pub hint_layers: Option<bool>,
}

#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct SharePickerOptions {
    pub allow_monitor: Option<bool>,
    pub allow_window: Option<bool>,
    pub allow_custom_region: Option<bool>,
    pub allow_restore_token_default: Option<bool>,
    pub dialog_parent_window_handle: Option<String>,
}
