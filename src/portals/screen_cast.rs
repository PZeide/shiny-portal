use std::collections::HashMap;

use enumflags2::{BitFlags, bitflags};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr as DeserializeRepr, Serialize_repr as SerializeRepr};
use tracing::{debug, info, warn};
use zbus::{Connection, interface};
use zvariant::{
    DeserializeDict, ObjectPath, OwnedObjectPath, OwnedValue, SerializeDict, Type, Value,
};

use crate::{
    common::{
        pipewire_cast::ScreencastThread,
        shell_ipc::{SelectionResult, SharePickerOptions, SharePickerResult, ShinyShell},
        wayland_capture::{CaptureTarget, DirectCapture},
    },
    config::Config,
    portals::{PortalResponse, request::Request, session::Session},
};

#[bitflags(default = Monitor | Window)]
#[derive(PartialEq, Eq, Copy, Clone, SerializeRepr, DeserializeRepr, Debug, Type)]
#[repr(u32)]
enum SourceType {
    Monitor = 1,
    Window = 2,
    Virtual = 4,
}

#[bitflags(default = Hidden)]
#[derive(PartialEq, Eq, Copy, Clone, SerializeRepr, DeserializeRepr, Debug, Type)]
#[repr(u32)]
enum CursorMode {
    Hidden = 1,
    Embedded = 2,
    Metadata = 4,
}

#[derive(Default, PartialEq, Eq, Copy, Clone, SerializeRepr, DeserializeRepr, Debug, Type)]
#[repr(u32)]
pub enum PersistMode {
    #[default]
    DoNot = 0,
    Application = 1,
    Explicit = 2,
}

const RESTORE_DATA_VENDOR: &str = "shiny";
const RESTORE_DATA_VERSION: u32 = 1;

#[derive(SerializeDict, Debug, Type)]
#[zvariant(signature = "a{sv}")]
struct CreateSessionResult {
    session_id: String,
}

#[derive(Serialize, Deserialize, Debug, Type)]
#[zvariant(signature = "(suv)")]
struct RestoreData {
    vendor: String,
    version: u32,
    data: OwnedValue,
}

#[derive(DeserializeDict, Debug, Type)]
#[zvariant(signature = "a{sv}")]
struct SelectSourcesOptions {
    types: Option<BitFlags<SourceType>>,
    multiple: Option<bool>,
    cursor_mode: Option<CursorMode>,
    restore_data: Option<RestoreData>,
    persist_mode: Option<PersistMode>,
}

#[derive(SerializeDict, Debug, Type)]
#[zvariant(signature = "a{sv}")]
struct StreamProperties {
    position: Option<(i32, i32)>,
    size: (i32, i32),
    source_type: SourceType,
    mapping_id: Option<String>,
    #[zvariant(rename = "pipewire-serial")]
    pipewire_serial: u64,
}

#[derive(Serialize, Debug, Type)]
struct Stream(u32, StreamProperties);

#[derive(SerializeDict, Debug, Type)]
#[zvariant(signature = "a{sv}")]
struct StartResult {
    streams: Vec<Stream>,
    persist_mode: Option<PersistMode>,
    restore_data: Option<RestoreData>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
enum RestoreTokenPayload {
    Monitor { monitor: String },
    Window { class: String, title: String },
}

#[derive(Debug)]
struct ScreenCastOptions {
    source_types: BitFlags<SourceType>,
    cursor_mode: CursorMode,
    persist_mode: PersistMode,
    restore_data: Option<RestoreData>,
}

impl Default for ScreenCastOptions {
    fn default() -> Self {
        Self {
            source_types: SourceType::Monitor | SourceType::Window,
            cursor_mode: CursorMode::Hidden,
            persist_mode: PersistMode::DoNot,
            restore_data: None,
        }
    }
}

struct ScreenCastSession {
    session_handle: OwnedObjectPath,
    options: ScreenCastOptions,
    cast_thread: Option<ScreencastThread>,
}

pub struct ScreenCastPortal {
    shell: ShinyShell,
    config: Config,
}

impl ScreenCastPortal {
    pub fn new(config: Config) -> Self {
        Self {
            shell: ShinyShell,
            config,
        }
    }
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastPortal {
    #[zbus(property)]
    async fn available_source_types(&self) -> u32 {
        (SourceType::Monitor | SourceType::Window).bits()
    }

    #[zbus(property)]
    async fn available_cursor_modes(&self) -> u32 {
        (CursorMode::Hidden | CursorMode::Embedded).bits()
    }

    #[zbus(property)]
    async fn version(&self) -> u32 {
        6
    }

    async fn create_session(
        &mut self,
        #[zbus(connection)] connection: &Connection,
        request_handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: String,
        _options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse<CreateSessionResult>> {
        info!("creating screencast session for app {app_id} at {session_handle}");

        Request::register(connection.object_server(), &request_handle).await?;

        Session::register(
            connection.object_server(),
            &session_handle,
            ScreenCastSession {
                session_handle: session_handle.to_owned().into(),
                options: ScreenCastOptions::default(),
                cast_thread: None,
            },
            |inner| {
                let session_handle = inner.session_handle.clone();
                let thread = inner.cast_thread.take();

                async move {
                    info!("removing screencast session {session_handle:?}");
                    if let Some(thread) = thread {
                        debug!("stopping active pw stream for {session_handle:?}");
                        thread.stop();
                    }
                }
            },
        )
        .await?;

        Ok(PortalResponse::Success(CreateSessionResult {
            session_id: session_handle.to_string(),
        }))
    }

    async fn select_sources(
        &self,
        #[zbus(connection)] connection: &Connection,
        request_handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: String,
        options: SelectSourcesOptions,
    ) -> zbus::fdo::Result<PortalResponse> {
        info!("selecting screencast sources for app {app_id} in {session_handle}");
        Request::register(connection.object_server(), &request_handle).await?;

        let Some(session_interface) =
            Session::<ScreenCastSession>::get(connection.object_server(), &session_handle).await
        else {
            warn!("session {session_handle:?} not found");
            return Ok(PortalResponse::Other);
        };

        let mut session = session_interface.get_mut().await;

        if let Some(types) = options.types {
            session.inner.options.source_types = types;
        }

        if options.multiple == Some(true) {
            warn!("multiple source selection is not supported; only one source will be returned");
        }

        if let Some(mode) = options.cursor_mode {
            if mode == CursorMode::Metadata {
                warn!("metadata cursor mode is unsupported; using embedded cursor mode");
                session.inner.options.cursor_mode = CursorMode::Embedded;
            } else {
                session.inner.options.cursor_mode = mode;
            }
        }

        if let Some(restore_data) = options.restore_data {
            session.inner.options.restore_data = Some(restore_data);
        }

        if let Some(mode) = options.persist_mode {
            session.inner.options.persist_mode = mode;
        }

        debug!(
            "session {session_handle:?} options updated: {:?}",
            session.inner.options
        );

        Ok(PortalResponse::Success(HashMap::new()))
    }

    async fn start(
        &mut self,
        #[zbus(connection)] connection: &Connection,
        request_handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: String,
        parent_window: String,
        _options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse<StartResult>> {
        info!("starting screencast for app {app_id} in {session_handle}");
        Request::register(connection.object_server(), &request_handle).await?;

        let Some(session_interface) =
            Session::<ScreenCastSession>::get(connection.object_server(), &session_handle).await
        else {
            warn!("session {session_handle:?} not found");
            return Ok(PortalResponse::Other);
        };

        let mut session = session_interface.get_mut().await;
        let options = &session.inner.options;
        let persist_mode = options.persist_mode;
        let overlay_cursor = options.cursor_mode == CursorMode::Embedded;

        let Ok(capture) = DirectCapture::connect() else {
            warn!("failed to create wayland capture connection");
            return Ok(PortalResponse::Other);
        };

        let restored_source = options
            .restore_data
            .as_ref()
            .and_then(decode_restore_data)
            .and_then(|payload| resolve_restore_payload(&capture, options.source_types, payload));

        let (target, source_type, restore_payload, allow_restore_token) =
            if let Some(restored_source) = restored_source {
                info!("restored screencast source from portal restore data");
                restored_source
            } else {
                let share_result = match self
                    .shell
                    .share_picker(SharePickerOptions {
                        allow_monitor: Some(options.source_types.contains(SourceType::Monitor)),
                        allow_window: Some(options.source_types.contains(SourceType::Window)),
                        allow_custom_region: Some(false),
                        allow_restore_token_default: restore_token_default(options),
                        dialog_parent_window_handle: Some(parent_window),
                    })
                    .await
                {
                    Ok(result) => result,
                    Err(err) => {
                        warn!("share picker failed: {err}");
                        return Ok(PortalResponse::Other);
                    }
                };

                let SharePickerResult::Selected {
                    result: selection, ..
                } = share_result
                else {
                    info!("share picker was cancelled");
                    return Ok(PortalResponse::Cancelled);
                };

                match selection {
                    SelectionResult::Monitor {
                        monitor,
                        allow_restore_token,
                    } => {
                        let Some(output) = capture.outputs().iter().find(|o| o.name == monitor)
                        else {
                            warn!("selected monitor {monitor} was not found");
                            return Ok(PortalResponse::Other);
                        };

                        info!("selected monitor source: {monitor}");
                        (
                            CaptureTarget::Output(output.output.clone()),
                            SourceType::Monitor,
                            RestoreTokenPayload::Monitor { monitor },
                            allow_restore_token,
                        )
                    }
                    SelectionResult::Window {
                        stable_id,
                        clazz,
                        title,
                        allow_restore_token,
                        ..
                    } => {
                        let Some(window) = capture
                            .toplevels()
                            .iter()
                            .find(|w| w.identifier == stable_id)
                        else {
                            warn!("selected window {stable_id} was not found");
                            return Ok(PortalResponse::Other);
                        };

                        info!("selected window source: {stable_id}");
                        (
                            CaptureTarget::Toplevel(window.handle.clone()),
                            SourceType::Window,
                            RestoreTokenPayload::Window {
                                class: clazz,
                                title,
                            },
                            allow_restore_token,
                        )
                    }
                    SelectionResult::Custom { region, .. } => {
                        warn!("custom region capture is not implemented: {region:?}");
                        return Ok(PortalResponse::Other);
                    }
                }
            };

        let cast_thread =
            ScreencastThread::start_cast(overlay_cursor, target, capture, self.config)
                .await
                .map_err(|err| {
                    zbus::Error::Failure(format!("cannot start PipeWire stream: {err}"))
                })?;

        let node_id = cast_thread.node_id();
        let pipewire_serial = cast_thread.pipewire_serial();
        let size = cast_thread.size();

        if let Some(previous) = session.inner.cast_thread.replace(cast_thread) {
            previous.stop();
        }

        info!("screencast started with pw node id {node_id}, serial {pipewire_serial}");

        let restore_data = if should_return_restore_token(persist_mode, allow_restore_token) {
            restore_data_from_payload(restore_payload)
        } else {
            None
        };

        let persist_mode = restore_data.as_ref().map(|_| persist_mode);

        Ok(PortalResponse::Success(StartResult {
            streams: vec![Stream(
                node_id,
                StreamProperties {
                    position: None,
                    size: (size.width as i32, size.height as i32),
                    source_type,
                    mapping_id: None,
                    pipewire_serial,
                },
            )],
            persist_mode,
            restore_data,
        }))
    }
}

fn restore_token_default(options: &ScreenCastOptions) -> Option<bool> {
    match options.persist_mode {
        PersistMode::DoNot => None,
        PersistMode::Application => Some(true),
        PersistMode::Explicit => Some(options.restore_data.is_some()),
    }
}

fn should_return_restore_token(persist_mode: PersistMode, allow_restore_token: bool) -> bool {
    persist_mode != PersistMode::DoNot && allow_restore_token
}

fn restore_data_from_payload(payload: RestoreTokenPayload) -> Option<RestoreData> {
    let data = match serde_json::to_string(&payload) {
        Ok(data) => data,
        Err(err) => {
            warn!("failed to serialize restore token payload: {err}");
            return None;
        }
    };

    Some(RestoreData {
        vendor: RESTORE_DATA_VENDOR.into(),
        version: RESTORE_DATA_VERSION,
        data: zvariant::OwnedValue::from(zvariant::Str::from(data)),
    })
}

fn decode_restore_data(restore_data: &RestoreData) -> Option<RestoreTokenPayload> {
    if restore_data.vendor != RESTORE_DATA_VENDOR || restore_data.version != RESTORE_DATA_VERSION {
        warn!(
            "ignoring unsupported restore data vendor={} version={}",
            restore_data.vendor, restore_data.version
        );
        return None;
    }

    let data = match restore_data.data.try_clone().and_then(String::try_from) {
        Ok(data) => data,
        Err(err) => {
            warn!("ignoring restore data with invalid payload type: {err}");
            return None;
        }
    };

    match serde_json::from_str(&data) {
        Ok(payload) => Some(payload),
        Err(err) => {
            warn!("ignoring malformed restore data payload: {err}");
            None
        }
    }
}

fn resolve_restore_payload(
    capture: &DirectCapture,
    source_types: BitFlags<SourceType>,
    payload: RestoreTokenPayload,
) -> Option<(CaptureTarget, SourceType, RestoreTokenPayload, bool)> {
    match payload {
        RestoreTokenPayload::Monitor { monitor } => {
            if !source_types.contains(SourceType::Monitor) {
                warn!("ignoring restored monitor because monitor capture was not requested");
                return None;
            }

            let Some(output) = capture
                .outputs()
                .iter()
                .find(|output| output.name == monitor)
            else {
                warn!("restored monitor {monitor} is no longer available");
                return None;
            };

            Some((
                CaptureTarget::Output(output.output.clone()),
                SourceType::Monitor,
                RestoreTokenPayload::Monitor { monitor },
                true,
            ))
        }
        RestoreTokenPayload::Window { class, title } => {
            if !source_types.contains(SourceType::Window) {
                warn!("ignoring restored window because window capture was not requested");
                return None;
            }

            if class.is_empty() {
                warn!("restored window token has no class");
                return None;
            }

            let class_matches: Vec<_> = capture
                .toplevels()
                .iter()
                .filter(|window| window.app_id == class)
                .collect();

            let window = match class_matches.as_slice() {
                [] => {
                    warn!("no current window matches restored class {class}");
                    return None;
                }
                [window] => *window,
                matches => {
                    let title_matches: Vec<_> = matches
                        .iter()
                        .copied()
                        .filter(|window| window.title == title)
                        .collect();

                    match title_matches.as_slice() {
                        [window] => *window,
                        [] => {
                            warn!(
                                "{} windows match restored class {class}, but none match title {title:?}",
                                matches.len()
                            );
                            return None;
                        }
                        matches => {
                            warn!(
                                "{} windows match restored class {class} and title {title:?}; prompting instead",
                                matches.len()
                            );

                            return None;
                        }
                    }
                }
            };

            info!(
                "restored window using class={} title={:?}",
                window.app_id, window.title
            );

            Some(restored_window(window, true))
        }
    }
}

fn restored_window(
    window: &crate::common::wayland_capture::ToplevelInfo,
    allow_restore_token: bool,
) -> (CaptureTarget, SourceType, RestoreTokenPayload, bool) {
    (
        CaptureTarget::Toplevel(window.handle.clone()),
        SourceType::Window,
        RestoreTokenPayload::Window {
            class: window.app_id.clone(),
            title: window.title.clone(),
        },
        allow_restore_token,
    )
}
