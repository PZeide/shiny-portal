use std::{
    os::fd::{AsFd, BorrowedFd},
    path::Path,
};

use drm::node::DrmNode;
use gbm::{BufferObject, BufferObjectFlags, Device as GbmDevice, Modifier};
use tracing::{debug, info, warn};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
};
use wayland_protocols::{
    ext::{
        foreign_toplevel_list::v1::client::{
            ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
            ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
        },
        image_capture_source::v1::client::{
            ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
            ext_image_capture_source_v1::ExtImageCaptureSourceV1,
            ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
        },
        image_copy_capture::v1::client::{
            ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
            ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
            ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
        },
    },
    wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
        zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};

pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub output: WlOutput,
    pub name: String,
    pub description: String,
    pub position: (i32, i32),
    pub size: Size,
}

#[derive(Debug, Clone)]
pub struct ToplevelInfo {
    pub handle: ExtForeignToplevelHandleV1,
    pub app_id: String,
    pub title: String,
    pub identifier: String,
    pub active: bool,
}

impl ToplevelInfo {
    fn new(handle: ExtForeignToplevelHandleV1) -> Self {
        Self {
            handle,
            app_id: String::new(),
            title: String::new(),
            identifier: String::new(),
            active: true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum CaptureTarget {
    Output(WlOutput),
    Toplevel(ExtForeignToplevelHandleV1),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmabufFormat {
    pub fourcc: u32,
    pub modifiers: Vec<u64>,
    pub size: Size,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmFormat {
    pub format: wl_shm::Format,
    pub size: Size,
    pub stride: u32,
}

impl ShmFormat {
    pub fn byte_size(&self) -> u64 {
        self.stride as u64 * self.size.height as u64
    }
}

#[derive(Debug, Clone)]
pub struct CaptureProbe {
    pub size: Size,
    pub shm_formats: Vec<ShmFormat>,
    pub dmabuf_formats: Vec<DmabufFormat>,
    pub has_dmabuf_device: bool,
}

const MAX_DAMAGE_RECTS: usize = 24;
const FULL_DAMAGE_AREA_PERCENT: u64 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl DamageRect {
    fn full(size: Size) -> Self {
        Self {
            x: 0,
            y: 0,
            width: size.width.min(i32::MAX as u32) as i32,
            height: size.height.min(i32::MAX as u32) as i32,
        }
    }

    fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    fn clipped_to(&self, size: Size) -> Option<Self> {
        if !self.is_valid() || size.width == 0 || size.height == 0 {
            return None;
        }

        let max_x = size.width.min(i32::MAX as u32) as i64;
        let max_y = size.height.min(i32::MAX as u32) as i64;
        let x1 = (self.x as i64).clamp(0, max_x);
        let y1 = (self.y as i64).clamp(0, max_y);
        let x2 = (self.x as i64 + self.width as i64).clamp(0, max_x);
        let y2 = (self.y as i64 + self.height as i64).clamp(0, max_y);

        if x2 <= x1 || y2 <= y1 {
            return None;
        }

        Some(Self {
            x: x1 as i32,
            y: y1 as i32,
            width: (x2 - x1) as i32,
            height: (y2 - y1) as i32,
        })
    }

    fn area(&self) -> u64 {
        if !self.is_valid() {
            return 0;
        }

        self.width as u64 * self.height as u64
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DamageSet {
    full: bool,
    rects: Vec<DamageRect>,
}

impl DamageSet {
    pub fn full() -> Self {
        Self {
            full: true,
            rects: Vec::new(),
        }
    }

    pub fn empty() -> Self {
        Self {
            full: false,
            rects: Vec::new(),
        }
    }

    pub fn is_full(&self) -> bool {
        self.full
    }

    pub fn add_many(&mut self, rects: &[DamageRect], frame_size: Size) {
        if self.full {
            return;
        }

        for rect in rects {
            if let Some(rect) = rect.clipped_to(frame_size) {
                self.rects.push(rect);
            }
        }
        self.simplify(frame_size);
    }

    pub fn rects_for_frame(&self, frame_size: Size) -> Vec<DamageRect> {
        if self.full {
            return vec![DamageRect::full(frame_size)];
        }

        self.rects
            .iter()
            .filter_map(|rect| rect.clipped_to(frame_size))
            .collect()
    }

    fn simplify(&mut self, frame_size: Size) {
        if self.full {
            return;
        }

        self.rects = self
            .rects
            .iter()
            .filter_map(|rect| rect.clipped_to(frame_size))
            .collect();

        if self.rects.len() > MAX_DAMAGE_RECTS {
            self.full = true;
            self.rects.clear();
            return;
        }

        let frame_area = frame_size.width as u64 * frame_size.height as u64;
        if frame_area == 0 {
            self.full = true;
            self.rects.clear();
            return;
        }

        let damaged_area = self
            .rects
            .iter()
            .map(DamageRect::area)
            .sum::<u64>()
            .min(frame_area);

        if damaged_area * 100 >= frame_area * FULL_DAMAGE_AREA_PERCENT {
            self.full = true;
            self.rects.clear();
        }
    }
}

#[derive(Debug)]
pub enum CaptureError {
    Anyhow(anyhow::Error),
    BufferConstraints,
    Stopped,
    Failed,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anyhow(err) => err.fmt(f),
            Self::BufferConstraints => f.write_str("buffer constraints changed"),
            Self::Stopped => f.write_str("capture target stopped"),
            Self::Failed => f.write_str("frame capture failed"),
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<anyhow::Error> for CaptureError {
    fn from(value: anyhow::Error) -> Self {
        Self::Anyhow(value)
    }
}

#[derive(Debug)]
pub struct DirectCapture {
    conn: Connection,
    globals: GlobalList,
    outputs: Vec<OutputInfo>,
    toplevels: Vec<ToplevelInfo>,
    gbm: Option<GbmDevice<Card>>,
}

impl DirectCapture {
    pub fn connect() -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env()?;
        let (globals, _) = registry_queue_init::<RegistryState>(&conn)?;
        let mut capture = Self {
            conn,
            globals,
            outputs: Vec::new(),
            toplevels: Vec::new(),
            gbm: None,
        };

        capture.refresh_outputs()?;
        capture.refresh_toplevels()?;
        Ok(capture)
    }

    pub fn outputs(&self) -> &[OutputInfo] {
        &self.outputs
    }

    pub fn toplevels(&self) -> &[ToplevelInfo] {
        &self.toplevels
    }

    pub fn refresh_outputs(&mut self) -> anyhow::Result<()> {
        let mut state = DiscoveryState::default();
        let mut event_queue = self.conn.new_event_queue::<DiscoveryState>();
        let qh = event_queue.handle();

        let xdg_output_manager = self
            .globals
            .bind::<ZxdgOutputManagerV1, _, _>(&qh, 3..=3, ())?;

        self.conn.display().get_registry(&qh, ());
        event_queue.roundtrip(&mut state)?;

        let xdg_outputs: Vec<ZxdgOutputV1> = state
            .outputs
            .iter()
            .enumerate()
            .map(|(index, output)| xdg_output_manager.get_xdg_output(&output.output, &qh, index))
            .collect();

        event_queue.roundtrip(&mut state)?;

        for xdg_output in xdg_outputs {
            xdg_output.destroy();
        }

        self.outputs = state.outputs;
        info!("discovered {} Wayland outputs", self.outputs.len());
        for output in &self.outputs {
            debug!(
                "output: name={}, description={}, size={}x{}",
                output.name, output.description, output.size.width, output.size.height
            );
        }

        Ok(())
    }

    pub fn refresh_toplevels(&mut self) -> anyhow::Result<()> {
        let mut state = CaptureState::new(true);
        let mut event_queue = self.conn.new_event_queue::<CaptureState>();
        let qh = event_queue.handle();

        let _list = self
            .globals
            .bind::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ())?;

        event_queue.roundtrip(&mut state)?;

        self.toplevels = state.toplevels;
        info!("discovered {} Wayland toplevels", self.toplevels.len());

        Ok(())
    }

    pub fn probe(
        &mut self,
        target: &CaptureTarget,
        paint_cursors: bool,
    ) -> anyhow::Result<CaptureProbe> {
        let (state, _event_queue, capture_frame) =
            self.create_frame(target, paint_cursors, true)?;
        capture_frame.destroy();
        self.conn.flush()?;

        if self.gbm.is_none() {
            self.gbm = state.gbm;
        }

        let size = state
            .dmabuf_formats
            .first()
            .map(|format| format.size)
            .or_else(|| state.shm_formats.first().map(|format| format.size))
            .unwrap_or(state.session_size);

        Ok(CaptureProbe {
            size,
            shm_formats: state.shm_formats,
            dmabuf_formats: state.dmabuf_formats,
            has_dmabuf_device: self.gbm.is_some(),
        })
    }

    pub fn create_dmabuf_buffer(
        &mut self,
        target: &CaptureTarget,
        preferred_fourcc: Option<u32>,
        preferred_modifier: Option<u64>,
    ) -> anyhow::Result<DirectCaptureBuffer> {
        let probe = self.probe(target, false)?;
        let Some(gbm) = self.gbm.as_ref() else {
            anyhow::bail!("compositor did not provide a DMA-BUF device");
        };

        let modifier = preferred_modifier
            .or_else(|| {
                probe
                    .dmabuf_formats
                    .iter()
                    .find_map(|format| format.modifiers.first().copied())
            })
            .ok_or_else(|| anyhow::anyhow!("no DMA-BUF modifiers were advertised"))?;

        let mut candidates = Vec::new();
        if let Some(fourcc) = preferred_fourcc {
            candidates.extend(
                probe
                    .dmabuf_formats
                    .iter()
                    .filter(|format| {
                        format.fourcc == fourcc && format.modifiers.contains(&modifier)
                    })
                    .cloned(),
            );
        } else {
            for format in &probe.dmabuf_formats {
                if format.modifiers.contains(&modifier) && !candidates.contains(format) {
                    candidates.push(format.clone());
                }
            }
        }

        let mut last_error = None;
        for format in candidates {
            match self.create_dmabuf_buffer_for_format(gbm, format.clone(), modifier) {
                Ok(buffer) => return Ok(buffer),
                Err(err) => {
                    warn!(
                        "failed to create DMA-BUF buffer for fourcc=0x{:08x}, \
                         size={}x{}, modifier=0x{:016x}: {err}",
                        format.fourcc, format.size.width, format.size.height, modifier
                    );
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no DMA-BUF formats were advertised")))
    }

    pub fn create_shm_buffer(
        &self,
        target: &CaptureTarget,
        preferred_format: Option<wl_shm::Format>,
        fd: BorrowedFd<'_>,
    ) -> anyhow::Result<DirectCaptureBuffer> {
        let (state, event_queue, capture_frame) = self.create_frame(target, false, false)?;
        capture_frame.destroy();
        self.conn.flush()?;

        let qh = event_queue.handle();
        let format = choose_shm_format(&state.shm_formats, preferred_format)
            .ok_or_else(|| anyhow::anyhow!("no compatible SHM format was advertised"))?;

        let shm = self.globals.bind::<WlShm, _, _>(&qh, 1..=1, ())?;
        let pool = shm.create_pool(fd, format.byte_size().try_into()?, &qh, ());
        let buffer = pool.create_buffer(
            0,
            format.size.width as i32,
            format.size.height as i32,
            format.stride as i32,
            format.format,
            &qh,
            (),
        );

        Ok(DirectCaptureBuffer::Shm {
            buffer,
            pool,
            format,
        })
    }

    pub fn capture_into_buffer(
        &self,
        target: &CaptureTarget,
        paint_cursors: bool,
        buffer: &DirectCaptureBuffer,
        damage: &DamageSet,
    ) -> Result<Vec<DamageRect>, CaptureError> {
        let (mut state, mut event_queue, capture_frame) =
            self.create_frame(target, paint_cursors, false)?;
        let frame = &capture_frame.frame;

        frame.attach_buffer(buffer.wl_buffer());

        let damage_rects = damage.rects_for_frame(buffer.size());
        if damage_rects.is_empty() {
            debug!("capturing frame with no pending client damage");
        } else if damage.is_full() {
            debug!(
                "capturing frame with full client damage at {}x{}",
                buffer.size().width,
                buffer.size().height
            );
        } else {
            debug!(
                "capturing frame with {} client damage rects",
                damage_rects.len()
            );
        }

        for rect in damage_rects {
            frame.damage_buffer(rect.x, rect.y, rect.width, rect.height);
        }

        frame.capture();

        loop {
            if let Some(frame_state) = state.frame_state {
                let result = match frame_state {
                    FrameState::Ready => Ok(state.damage_rects),
                    FrameState::Failed(WEnum::Value(FailureReason::BufferConstraints)) => {
                        Err(CaptureError::BufferConstraints)
                    }
                    FrameState::Failed(WEnum::Value(FailureReason::Stopped)) => {
                        Err(CaptureError::Stopped)
                    }
                    FrameState::Failed(_) => Err(CaptureError::Failed),
                };

                capture_frame.destroy();
                self.conn
                    .flush()
                    .map_err(|err| CaptureError::Anyhow(err.into()))?;

                return result;
            }

            if let Err(err) = event_queue.blocking_dispatch(&mut state) {
                capture_frame.destroy();
                let _ = self.conn.flush();
                return Err(CaptureError::Anyhow(err.into()));
            }
        }
    }

    fn create_frame(
        &self,
        target: &CaptureTarget,
        paint_cursors: bool,
        find_gbm: bool,
    ) -> anyhow::Result<(CaptureState, EventQueue<CaptureState>, CaptureFrame)> {
        let mut state = CaptureState::new(find_gbm);
        let mut event_queue = self.conn.new_event_queue::<CaptureState>();
        let qh = event_queue.handle();

        let manager = self
            .globals
            .bind::<ExtImageCopyCaptureManagerV1, _, _>(&qh, 1..=1, ())?;

        let source = match target {
            CaptureTarget::Output(output) => {
                let source_manager = self
                    .globals
                    .bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())?;
                let source = source_manager.create_source(output, &qh, ());
                source_manager.destroy();
                source
            }
            CaptureTarget::Toplevel(toplevel) => {
                let source_manager = self
                    .globals
                    .bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())?;
                let source = source_manager.create_source(toplevel, &qh, ());
                source_manager.destroy();
                source
            }
        };

        let options = if paint_cursors {
            Options::PaintCursors
        } else {
            Options::empty()
        };

        let session = manager.create_session(&source, options, &qh, ());
        source.destroy();
        manager.destroy();

        let frame = session.create_frame(&qh, ());

        while !state.session_done {
            if let Err(err) = event_queue.blocking_dispatch(&mut state) {
                frame.destroy();
                session.destroy();
                let _ = self.conn.flush();
                return Err(err.into());
            }
        }

        Ok((state, event_queue, CaptureFrame { frame, session }))
    }

    fn create_dmabuf_buffer_for_format(
        &self,
        gbm: &GbmDevice<Card>,
        format: DmabufFormat,
        modifier: u64,
    ) -> anyhow::Result<DirectCaptureBuffer> {
        let mut state = CaptureState::new(false);
        let mut event_queue = self.conn.new_event_queue::<CaptureState>();
        let qh = event_queue.handle();
        let linux_dmabuf = self.globals.bind::<ZwpLinuxDmabufV1, _, _>(
            &qh,
            4..=ZwpLinuxDmabufV1::interface().version,
            (),
        )?;

        let gbm_format = gbm::Format::try_from(format.fourcc)?;
        let bo = if modifier == DRM_FORMAT_MOD_INVALID {
            gbm.create_buffer_object::<()>(
                format.size.width,
                format.size.height,
                gbm_format,
                BufferObjectFlags::RENDERING,
            )?
        } else {
            gbm.create_buffer_object_with_modifiers2::<()>(
                format.size.width,
                format.size.height,
                gbm_format,
                [Modifier::from(modifier)].into_iter(),
                BufferObjectFlags::RENDERING,
            )?
        };

        let actual_modifier: u64 = bo.modifier().into();
        if actual_modifier != modifier {
            anyhow::bail!(
                "GBM returned modifier 0x{actual_modifier:016x}, expected 0x{modifier:016x}"
            );
        }

        let actual_fourcc = bo.format() as u32;
        if actual_fourcc != format.fourcc {
            anyhow::bail!(
                "GBM returned fourcc=0x{:08x}, expected 0x{:08x}",
                actual_fourcc,
                format.fourcc
            );
        }

        let params = linux_dmabuf.create_params(&qh, ());
        let plane_count = bo.plane_count();
        let mut plane_fds = Vec::new();
        for plane in 0..plane_count {
            let fd = bo.fd_for_plane(plane as i32)?;
            params.add(
                fd.as_fd(),
                plane,
                bo.offset(plane as i32),
                bo.stride_for_plane(plane as i32),
                (modifier >> 32) as u32,
                (modifier & 0xffffffff) as u32,
            );
            plane_fds.push(fd);
        }

        params.create(
            format.size.width as i32,
            format.size.height as i32,
            format.fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
        );

        let buffer = loop {
            if let Some(result) = state.buffer_create.take() {
                break match result {
                    BufferCreateState::Created(buffer) => buffer,
                    BufferCreateState::Failed => {
                        anyhow::bail!(
                            "compositor rejected DMA-BUF buffer for fourcc=0x{:08x}, \
                             size={}x{}, modifier=0x{:016x}",
                            format.fourcc,
                            format.size.width,
                            format.size.height,
                            modifier
                        );
                    }
                };
            }

            event_queue.blocking_dispatch(&mut state)?;
        };

        drop(plane_fds);

        info!(
            "created direct DMA-BUF wl_buffer: fourcc=0x{:08x}, size={}x{}, planes={}, modifier=0x{:016x}",
            format.fourcc, format.size.width, format.size.height, plane_count, modifier
        );

        Ok(DirectCaptureBuffer::Dmabuf {
            buffer,
            bo,
            format,
            modifier,
        })
    }
}

struct CaptureFrame {
    frame: ExtImageCopyCaptureFrameV1,
    session: ExtImageCopyCaptureSessionV1,
}

impl CaptureFrame {
    fn destroy(self) {
        self.frame.destroy();
        self.session.destroy();
    }
}

pub enum DirectCaptureBuffer {
    Dmabuf {
        buffer: WlBuffer,
        bo: BufferObject<()>,
        format: DmabufFormat,
        modifier: u64,
    },
    Shm {
        buffer: WlBuffer,
        pool: WlShmPool,
        format: ShmFormat,
    },
}

impl DirectCaptureBuffer {
    pub fn wl_buffer(&self) -> &WlBuffer {
        match self {
            Self::Dmabuf { buffer, .. } | Self::Shm { buffer, .. } => buffer,
        }
    }

    pub fn size(&self) -> Size {
        match self {
            Self::Dmabuf { format, .. } => format.size,
            Self::Shm { format, .. } => format.size,
        }
    }

    pub fn dmabuf_bo(&self) -> Option<&BufferObject<()>> {
        match self {
            Self::Dmabuf { bo, .. } => Some(bo),
            Self::Shm { .. } => None,
        }
    }

    pub fn dmabuf_format(&self) -> Option<DmabufFormat> {
        match self {
            Self::Dmabuf { format, .. } => Some(format.clone()),
            Self::Shm { .. } => None,
        }
    }

    pub fn modifier(&self) -> Option<u64> {
        match self {
            Self::Dmabuf { modifier, .. } => Some(*modifier),
            Self::Shm { .. } => None,
        }
    }

    pub fn shm_format(&self) -> Option<ShmFormat> {
        match self {
            Self::Dmabuf { .. } => None,
            Self::Shm { format, .. } => Some(*format),
        }
    }
}

impl Drop for DirectCaptureBuffer {
    fn drop(&mut self) {
        match self {
            Self::Dmabuf { buffer, .. } => buffer.destroy(),
            Self::Shm { buffer, pool, .. } => {
                buffer.destroy();
                pool.destroy();
            }
        }
    }
}

#[derive(Default)]
struct DiscoveryState {
    outputs: Vec<OutputInfo>,
}

impl Dispatch<WlRegistry, ()> for DiscoveryState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "wl_output"
            && version >= 4
        {
            let output = registry.bind::<WlOutput, _, _>(name, 4, qh, ());
            state.outputs.push(OutputInfo {
                output,
                name: String::new(),
                description: String::new(),
                position: (0, 0),
                size: Size::default(),
            });
        }
    }
}

impl Dispatch<WlOutput, ()> for DiscoveryState {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state
            .outputs
            .iter_mut()
            .find(|candidate| candidate.output == *output)
        else {
            return;
        };

        match event {
            wl_output::Event::Name { name } => info.name = name,
            wl_output::Event::Description { description } => info.description = description,
            wl_output::Event::Mode { width, height, .. } => {
                info.size = Size {
                    width: width as u32,
                    height: height as u32,
                };
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for DiscoveryState {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.outputs.get_mut(*index) else {
            return;
        };

        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                info.position = (x, y);
            }
            zxdg_output_v1::Event::LogicalSize { width, height } if width > 0 && height > 0 => {
                info.size = Size {
                    width: width as u32,
                    height: height as u32,
                };
            }
            zxdg_output_v1::Event::Name { name } if info.name.is_empty() => info.name = name,
            zxdg_output_v1::Event::Description { description } if info.description.is_empty() => {
                info.description = description
            }
            _ => {}
        }
    }
}

delegate_noop!(DiscoveryState: ignore ZxdgOutputManagerV1);

#[derive(Debug, Copy, Clone)]
enum FrameState {
    Ready,
    Failed(WEnum<FailureReason>),
}

enum BufferCreateState {
    Created(WlBuffer),
    Failed,
}

struct CaptureState {
    shm_formats: Vec<ShmFormat>,
    dmabuf_formats: Vec<DmabufFormat>,
    frame_state: Option<FrameState>,
    session_done: bool,
    session_size: Size,
    gbm: Option<GbmDevice<Card>>,
    find_gbm: bool,
    toplevels: Vec<ToplevelInfo>,
    buffer_create: Option<BufferCreateState>,
    damage_rects: Vec<DamageRect>,
}

impl CaptureState {
    fn new(find_gbm: bool) -> Self {
        Self {
            shm_formats: Vec::new(),
            dmabuf_formats: Vec::new(),
            frame_state: None,
            session_done: false,
            session_size: Size::default(),
            gbm: None,
            find_gbm,
            toplevels: Vec::new(),
            buffer_create: None,
            damage_rects: Vec::new(),
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.session_size = Size { width, height };

                for format in &mut state.shm_formats {
                    format.size = state.session_size;
                    format.stride = width * 4;
                }

                for format in &mut state.dmabuf_formats {
                    format.size = state.session_size;
                }
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(format),
            } => {
                state.shm_formats.push(ShmFormat {
                    format,
                    size: state.session_size,
                    stride: state.session_size.width * 4,
                });
            }
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                if !state.find_gbm {
                    return;
                }

                let Ok(bytes) = <[u8; 8]>::try_from(device.as_slice()) else {
                    warn!("compositor sent malformed DMA-BUF device identifier");
                    return;
                };

                let dev_id = u64::from_le_bytes(bytes);
                let Ok(node) = DrmNode::from_dev_id(dev_id) else {
                    warn!("failed to resolve DRM node for dev id {dev_id}");
                    return;
                };

                let Some(path) = node.dev_path() else {
                    warn!("DRM node for dev id {dev_id} has no path");
                    return;
                };

                match Card::open(&path).and_then(|card| {
                    GbmDevice::new(card).map_err(|err| std::io::Error::other(err.to_string()))
                }) {
                    Ok(gbm) => {
                        info!("using compositor DMA-BUF device {}", path.display());
                        state.gbm = Some(gbm);
                    }
                    Err(err) => warn!("failed to open GBM device {}: {err}", path.display()),
                }
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                let modifiers = modifiers
                    .chunks_exact(8)
                    .filter_map(|bytes| match bytes.try_into() {
                        Ok(bytes) => Some(u64::from_ne_bytes(bytes)),
                        Err(err) => {
                            warn!("failed to parse DMA-BUF modifier bytes: {err}");
                            None
                        }
                    })
                    .collect();
                state.dmabuf_formats.push(DmabufFormat {
                    fourcc: format,
                    modifiers,
                    size: state.session_size,
                });
            }
            ext_image_copy_capture_session_v1::Event::Done => state.session_done = true,
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.session_done = true;
                state.frame_state = Some(FrameState::Failed(WEnum::Value(FailureReason::Stopped)));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.frame_state = Some(FrameState::Ready);
            }
            ext_image_copy_capture_frame_v1::Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                state.damage_rects.push(DamageRect {
                    x,
                    y,
                    width,
                    height,
                });
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.frame_state = Some(FrameState::Failed(reason));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(ToplevelInfo::new(toplevel));
        }
    }

    wayland_client::event_created_child!(CaptureState, ExtForeignToplevelHandleV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ())
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        toplevel: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state
            .toplevels
            .iter_mut()
            .find(|candidate| candidate.handle == *toplevel)
        else {
            return;
        };

        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => info.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => info.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                info.identifier = identifier
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => info.active = false,
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Failed => {
                state.buffer_create = Some(BufferCreateState::Failed);
            }
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                state.buffer_create = Some(BufferCreateState::Created(buffer));
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(CaptureState, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ())
    ]);
}

delegate_noop!(CaptureState: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(CaptureState: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(CaptureState: ignore ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(CaptureState: ignore ExtImageCaptureSourceV1);
delegate_noop!(CaptureState: ignore ZwpLinuxDmabufV1);
delegate_noop!(CaptureState: ignore WlShm);
delegate_noop!(CaptureState: ignore WlShmPool);
delegate_noop!(CaptureState: ignore WlBuffer);

struct RegistryState;
delegate_noop!(RegistryState: ignore ZwpLinuxDmabufV1);

impl Dispatch<WlRegistry, GlobalListContents> for RegistryState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub(crate) struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}

impl Card {
    fn open<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        Ok(Self(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?,
        ))
    }
}

fn choose_shm_format(
    formats: &[ShmFormat],
    preferred_format: Option<wl_shm::Format>,
) -> Option<ShmFormat> {
    if let Some(preferred_format) = preferred_format
        && let Some(format) = formats
            .iter()
            .find(|format| format.format == preferred_format)
            .copied()
    {
        return Some(format);
    }

    let preferred = [
        wl_shm::Format::Xrgb8888,
        wl_shm::Format::Argb8888,
        wl_shm::Format::Xbgr8888,
        wl_shm::Format::Abgr8888,
        wl_shm::Format::Xrgb2101010,
        wl_shm::Format::Argb2101010,
        wl_shm::Format::Xbgr2101010,
        wl_shm::Format::Abgr2101010,
    ];

    preferred
        .into_iter()
        .find_map(|candidate| {
            formats
                .iter()
                .find(|format| format.format == candidate)
                .copied()
        })
        .or_else(|| formats.first().copied())
}

pub const fn fourcc(code: &str) -> u32 {
    let bytes = code.as_bytes();
    (bytes[0] as u32)
        | ((bytes[1] as u32) << 8)
        | ((bytes[2] as u32) << 16)
        | ((bytes[3] as u32) << 24)
}
