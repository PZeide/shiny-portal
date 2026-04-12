use std::collections::HashMap;

use enumflags2::{BitFlags, bitflags};
use serde::Serialize;
use serde_repr::{Deserialize_repr as DeserializeRepr, Serialize_repr as SerializeRepr};
use tracing::{debug, error, info, warn};
use zbus::{Connection, interface};
use zvariant::{
    DeserializeDict, ObjectPath, OwnedObjectPath, OwnedValue, SerializeDict, Type, Value,
};

use crate::{
    common::{
        pipewire_cast::{CastTarget, ScreencastThread},
        shell_ipc::{SelectionResult, SharePickerOptions, SharePickerResult, ShinyShell},
    },
    portals::{PortalResponse, request::Request, session::Session},
};

#[bitflags]
#[derive(PartialEq, Eq, Copy, Clone, SerializeRepr, DeserializeRepr, Debug, Type)]
#[repr(u32)]
enum SourceType {
    Monitor = 1,
    Window = 2,
    Virtual = 4,
}

#[bitflags]
#[derive(PartialEq, Eq, Copy, Clone, SerializeRepr, DeserializeRepr, Debug, Type)]
#[repr(u32)]
enum CursorMode {
    Hidden = 1,
    Embedded = 2,
    Metadata = 4,
}

#[derive(PartialEq, Eq, Copy, Clone, SerializeRepr, DeserializeRepr, Debug, Type)]
#[repr(u32)]
pub enum PersistMode {
    DoNot = 0,
    Application = 1,
    Explicit = 2,
}

#[derive(SerializeDict, Debug, Type)]
#[zvariant(signature = "a{sv}")]
struct CreateSessionResult {
    session_id: String,
}

#[derive(SerializeDict, DeserializeDict, Debug, Type)]
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
struct ScreenCastRequest {
    session_handle: OwnedObjectPath,
    source_types: BitFlags<SourceType>,
    cursor_mode: CursorMode,
    persist_mode: PersistMode,
    restore_data: Option<RestoreData>,
}

struct ScreenCastSession {
    session_handle: OwnedObjectPath,
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
        (SourceType::Monitor | SourceType::Window | SourceType::Virtual).bits()
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
        info!("creating screenshare session for app {app_id} (session: {session_handle})");

        Request::register(
            connection.object_server(),
            &request_handle,
            ScreenCastRequest {
                session_handle: session_handle.to_owned().into(),
                source_types: BitFlags::from(SourceType::Monitor),
                cursor_mode: CursorMode::Hidden,
                restore_data: None,
                persist_mode: PersistMode::DoNot,
            },
            |inner| {
                let session_handle = inner.session_handle.clone();
                async move {
                    debug!("removing request of session {session_handle:?}");
                }
            },
        )
        .await?;

        Session::register(
            connection.object_server(),
            &session_handle,
            ScreenCastSession {
                session_handle: session_handle.to_owned().into(),
                cast_thread: None,
            },
            |inner| {
                let session_handle = inner.session_handle.clone();
                let thread = inner.cast_thread.take();

                async move {
                    debug!("removing session {session_handle:?}");

                    if let Some(thread) = thread {
                        debug!("screencast session found, stopping thread");
                        thread.stop();
                    }
                }
            },
        )
        .await?;

        debug!("session created for {app_id} at {session_handle:?} (request: {request_handle:?})");
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
        info!("configuring sources for app {app_id} (session: {session_handle})");

        let Some(request_interface) =
            Request::<ScreenCastRequest>::get(connection.object_server(), &request_handle).await
        else {
            info!("request with handle {request_handle:?} not found");
            return Ok(PortalResponse::Other);
        };

        let request = &mut request_interface.get_mut().await.inner;

        if let Some(types) = options.types {
            request.source_types = types;
        }

        if let Some(_) = options.multiple {
            warn!("option 'multiple' is unsupported for request {request_handle:?}");
        }

        if let Some(mode) = options.cursor_mode {
            if mode == CursorMode::Metadata {
                warn!(
                    "option 'cursor_mode' with value 'Metadata' is unsupported for request {request_handle:?}"
                );
            } else {
                request.cursor_mode = mode;
            }
        }

        if let Some(restore_data) = options.restore_data {
            request.restore_data = Some(restore_data);
        }

        if let Some(mode) = options.persist_mode {
            request.persist_mode = mode;
        }

        debug!("request {request_handle:?} updated {request:?}");
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
        info!("starting screen cast for app {app_id} (session: {session_handle})");

        let Some(request_interface) =
            Request::<ScreenCastRequest>::get(connection.object_server(), &request_handle).await
        else {
            info!("request with handle {request_handle:?} not found");
            return Ok(PortalResponse::Other);
        };

        let Some(session_interface) =
            Session::<ScreenCastSession>::get(connection.object_server(), &session_handle).await
        else {
            info!("session with handle {session_handle:?} not found");
            return Ok(PortalResponse::Other);
        };

        let mut request = request_interface.get_mut().await;

        let share_result = match self
            .shell
            .share_picker(SharePickerOptions {
                allow_monitor: Some(request.inner.source_types.contains(SourceType::Monitor)),
                allow_window: Some(request.inner.source_types.contains(SourceType::Window)),
                allow_custom_region: Some(request.inner.source_types.contains(SourceType::Virtual)),
                allow_restore_token_default: None,
                dialog_parent_window_handle: Some(parent_window),
            })
            .await
        {
            Ok(result) => result,
            Err(err) => {
                warn!("share picker failed: {err:?}");
                return Ok(PortalResponse::Other);
            }
        };

        let SharePickerResult::Selected {
            result: selection, ..
        } = share_result
        else {
            info!("share picker cancelled");
            return Ok(PortalResponse::Cancelled);
        };

        let overlay_cursor = request.inner.cursor_mode == CursorMode::Embedded;

        request.close(connection.object_server()).await?;
        drop(request);

        let mut session = session_interface.get_mut().await;

        let Ok(connection) = libwayshot::WayshotConnection::new() else {
            error!("failed to create libwayshot connection");
            return Ok(PortalResponse::Other);
        };

        let target = match selection {
            SelectionResult::Monitor { monitor, .. } => {
                let Some(output) = connection
                    .get_all_outputs()
                    .into_iter()
                    .find(|o| o.name == monitor)
                else {
                    warn!("cannot find selected monitor {monitor}");
                    return Ok(PortalResponse::Other);
                };

                CastTarget::Screen(output.wl_output.clone())
            }
            SelectionResult::Window { stable_id, .. } => {
                let Some(window) = connection
                    .get_all_toplevels()
                    .into_iter()
                    .find(|w| w.identifier == stable_id)
                else {
                    warn!("cannot find selected window {stable_id}");
                    return Ok(PortalResponse::Other);
                };

                CastTarget::TopLevel(window.handle.clone())
            }
            SelectionResult::Custom { .. } => {
                // FIXME Later
                return Ok(PortalResponse::Other);
            }
        };

        let cast_thread = ScreencastThread::start_cast(overlay_cursor, None, target, connection)
            .await
            .map_err(|e| {
                zbus::Error::Failure(format!("cannot start pipewire stream, error: {e}"))
            })?;

        let node_id = cast_thread.node_id();
        session.inner.cast_thread = Some(cast_thread);

        Ok(PortalResponse::Success(StartResult {
            streams: vec![Stream(
                node_id,
                StreamProperties {
                    position: None,
                    size: (1920, 1080),
                    source_type: SourceType::Monitor,
                    mapping_id: None,
                },
            )],
            persist_mode: None,
            restore_data: None,
        }))
    }
}
