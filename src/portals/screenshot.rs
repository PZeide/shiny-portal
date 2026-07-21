use anyhow::Context;
use enumflags2::bitflags;
use serde_repr::{Deserialize_repr as DeserializeRepr, Serialize_repr as SerializeRepr};
use tracing::{debug, info, warn};
use zbus::{Connection, interface};
use zvariant::{DeserializeDict, ObjectPath, SerializeDict, Type};

use crate::{
    common::{
        screenshot_capture::{capture_region, write_image},
        shell_ipc::{RegionSelectorOptions, RegionSelectorResult, ShinyShell},
        wayland_capture::DirectCapture,
    },
    portals::{PortalResponse, request::Request},
};

#[bitflags]
#[derive(PartialEq, Eq, Copy, Clone, SerializeRepr, DeserializeRepr, Debug, Type)]
#[repr(u32)]
enum ScreenshotTarget {
    Screen = 1,
    Window = 2,
    Area = 4,
    ActiveWindow = 8,
}

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
struct ScreenshotOptions {
    modal: Option<bool>,
    interactive: Option<bool>,
    target: Option<ScreenshotTarget>,
    permission_store_checked: Option<bool>,
}

#[derive(Debug, SerializeDict, Type)]
#[zvariant(signature = "a{sv}")]
struct ScreenshotResult {
    uri: String,
}

#[derive(Default)]
pub struct ScreenshotPortal {
    shell: ShinyShell,
}

#[interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl ScreenshotPortal {
    #[zbus(property)]
    async fn available_targets(&self) -> u32 {
        ScreenshotTarget::Area as u32
    }

    #[zbus(property)]
    async fn version(&self) -> u32 {
        3
    }

    async fn screenshot(
        &self,
        #[zbus(connection)] connection: &Connection,
        request_handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        options: ScreenshotOptions,
    ) -> zbus::fdo::Result<PortalResponse<ScreenshotResult>> {
        info!("taking screenshot for app {app_id}");
        Request::register(connection.object_server(), &request_handle).await?;

        debug!(
            "screenshot options: modal={:?}, interactive={:?}, target={:?}, permission_store_checked={:?}",
            options.modal, options.interactive, options.target, options.permission_store_checked
        );

        if let Some(target) = options.target
            && target != ScreenshotTarget::Area
        {
            warn!(
                "screenshot target {target:?} requested, but only area selection is supported; continuing with area selection"
            );
        }

        let uri = match self.capture_area().await {
            Ok(Some(uri)) => uri,
            Ok(None) => return Ok(PortalResponse::Cancelled),
            Err(err) => {
                warn!("screenshot failed: {err:#}");
                return Ok(PortalResponse::Other);
            }
        };

        info!("screenshot saved to {uri}");
        Ok(PortalResponse::Success(ScreenshotResult { uri }))
    }
}

impl ScreenshotPortal {
    async fn capture_area(&self) -> anyhow::Result<Option<String>> {
        let Some(region) = self.select_area().await.context("area selection failed")? else {
            return Ok(None);
        };

        let mut capture = DirectCapture::connect().context("Wayland connection failed")?;
        let image = capture_region(&mut capture, &region, false).context("capture failed")?;
        let uri = write_image(&image).context("writing the image failed")?;

        Ok(Some(uri))
    }

    async fn select_area(&self) -> anyhow::Result<Option<crate::common::shell_ipc::CustomRegion>> {
        let result = self
            .shell
            .region_selector(RegionSelectorOptions {
                freeze: Some(false),
                hint_windows: Some(true),
                hint_layers: Some(true),
            })
            .await?;

        Ok(match result {
            RegionSelectorResult::Selected { result, .. } => Some(result),
            RegionSelectorResult::Cancelled { .. } => None,
        })
    }
}
