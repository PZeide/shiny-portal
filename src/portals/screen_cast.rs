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

#[derive(Default)]
pub struct ScreenCastPortal {
    shell: ShinyShell,
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
        5
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

        Request::register(connection.object_server(), &request_handle, (), |_| async {
        })
        .await?;

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
                    debug!("removing screencast session {session_handle:?}");
                    if let Some(thread) = thread {
                        debug!("stopping active PipeWire stream for {session_handle:?}");
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
        Request::register(connection.object_server(), &request_handle, (), |_| async {
        })
        .await?;

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
                warn!("metadata cursor mode is unsupported; keeping previous cursor mode");
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
        Request::register(connection.object_server(), &request_handle, (), |_| async {
        })
        .await?;

        let Some(session_interface) =
            Session::<ScreenCastSession>::get(connection.object_server(), &session_handle).await
        else {
            warn!("session {session_handle:?} not found");
            return Ok(PortalResponse::Other);
        };

        let mut session = session_interface.get_mut().await;
        let options = &session.inner.options;

        let share_result = match self
            .shell
            .share_picker(SharePickerOptions {
                allow_monitor: Some(options.source_types.contains(SourceType::Monitor)),
                allow_window: Some(options.source_types.contains(SourceType::Window)),
                allow_custom_region: Some(false),
                allow_restore_token_default: None,
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

        let overlay_cursor = options.cursor_mode == CursorMode::Embedded;

        let Ok(capture) = DirectCapture::connect() else {
            warn!("failed to create direct Wayland capture connection");
            return Ok(PortalResponse::Other);
        };

        let (target, source_type) = match selection {
            SelectionResult::Monitor { monitor, .. } => {
                let Some(output) = capture
                    .outputs()
                    .iter()
                    .find(|o| o.name == monitor)
                else {
                    warn!("selected monitor {monitor} was not found");
                    return Ok(PortalResponse::Other);
                };

                info!("selected monitor source: {monitor}");
                (
                    CaptureTarget::Output(output.output.clone()),
                    SourceType::Monitor,
                )
            }
            SelectionResult::Window { stable_id, .. } => {
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
                )
            }
            SelectionResult::Custom { region, .. } => {
                warn!("custom region capture is not implemented: {region:?}");
                return Ok(PortalResponse::Other);
            }
        };

        let cast_thread = ScreencastThread::start_cast(overlay_cursor, target, capture)
            .await
            .map_err(|err| zbus::Error::Failure(format!("cannot start PipeWire stream: {err}")))?;

        let node_id = cast_thread.node_id();
        let size = cast_thread.size();
        if let Some(previous) = session.inner.cast_thread.replace(cast_thread) {
            previous.stop();
        }

        info!("screencast started with PipeWire node id {node_id}");

        Ok(PortalResponse::Success(StartResult {
            streams: vec![Stream(
                node_id,
                StreamProperties {
                    position: None,
                    size: (size.width as i32, size.height as i32),
                    source_type,
                    mapping_id: None,
                },
            )],
            persist_mode: None,
            restore_data: None,
        }))
    }
}
