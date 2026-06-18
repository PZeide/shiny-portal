use std::{
    ffi::c_void,
    io,
    os::fd::{AsFd, IntoRawFd},
    slice,
};

use pipewire::{
    spa::{
        self,
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{self, Pod, serialize::PodSerializer},
        sys as spa_sys,
    },
    stream::StreamState,
};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use wayland_client::protocol::wl_shm;

use crate::common::wayland_capture::{
    CaptureError, CaptureProbe, CaptureTarget, DamageRect, DamageSet, DirectCapture,
    DirectCaptureBuffer, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DmabufFormat, Size, fourcc,
};

pub struct ScreencastThread {
    node_id: u32,
    size: Size,
    thread_stop_tx: pipewire::channel::Sender<()>,
}

impl ScreencastThread {
    pub async fn start_cast(
        overlay_cursor: bool,
        target: CaptureTarget,
        capture: DirectCapture,
    ) -> anyhow::Result<Self> {
        let (node_id_tx, node_id_rx) = oneshot::channel();
        let (thread_stop_tx, thread_stop_rx) = pipewire::channel::channel::<()>();

        std::thread::spawn(move || {
            match start_stream(capture, overlay_cursor, target) {
                Ok((main_loop, listener, _stream, context, node_id_ready)) => {
                    let _ = node_id_tx.send(Ok(node_id_ready));
                    let weak_loop = main_loop.downgrade();
                    let _receiver = thread_stop_rx.attach(main_loop.loop_(), move |()| {
                        if let Some(main_loop) = weak_loop.upgrade() {
                            main_loop.quit();
                        }
                    });
                    main_loop.run();
                    drop(listener);
                    drop(context);
                }
                Err(err) => {
                    let _ = node_id_tx.send(Err(err));
                }
            }
        });

        let (node_id, size) = node_id_rx.await??.await??;
        Ok(Self {
            node_id,
            size,
            thread_stop_tx,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn stop(&self) {
        let _ = self.thread_stop_tx.send(());
    }
}

#[derive(Debug, Clone)]
struct StreamFormatState {
    size: Size,
    spa_formats: Vec<VideoFormat>,
    modifiers: Vec<u64>,
    dmabuf_supported: bool,
}

impl StreamFormatState {
    fn from_probe(probe: CaptureProbe, allow_shm_fallback: bool) -> anyhow::Result<Self> {
        let dmabuf_available = probe.has_dmabuf_device && !probe.dmabuf_formats.is_empty();
        if !dmabuf_available && !allow_shm_fallback {
            anyhow::bail!("compositor did not advertise a usable DMA-BUF capture path");
        }

        let mut spa_formats = Vec::new();
        let mut modifiers = Vec::new();
        if dmabuf_available {
            for format in preferred_dmabuf_formats(&probe.dmabuf_formats) {
                let preferred = preferred_modifiers(&format.modifiers);
                if preferred.is_empty() {
                    continue;
                }

                if let Some(spa_format) = drm_fourcc_to_spa(format.fourcc)
                    && !spa_formats.contains(&spa_format)
                {
                    spa_formats.push(spa_format);
                }
                for modifier in preferred {
                    if !modifiers.contains(&modifier) {
                        modifiers.push(modifier);
                    }
                }
            }
        }

        let dmabuf_supported = !spa_formats.is_empty() && !modifiers.is_empty();
        if !dmabuf_supported && !allow_shm_fallback {
            anyhow::bail!("compositor did not advertise usable DMA-BUF formats and modifiers");
        }

        if spa_formats.is_empty() && allow_shm_fallback {
            for format in &probe.shm_formats {
                if let Some(spa_format) = shm_format_to_spa(format.format)
                    && !spa_formats.contains(&spa_format)
                {
                    spa_formats.push(spa_format);
                }
            }
        }

        if spa_formats.is_empty() {
            anyhow::bail!("no capture formats could be mapped to PipeWire SPA video formats");
        }

        Ok(Self {
            size: probe.size,
            spa_formats,
            modifiers,
            dmabuf_supported,
        })
    }
}

struct StreamingData {
    capture: DirectCapture,
    target: CaptureTarget,
    overlay_cursor: bool,
    formats: StreamFormatState,
    chosen_format: Option<VideoFormat>,
    chosen_modifier: Option<u64>,
    allow_shm_fallback: bool,
    buffers: Vec<*mut pipewire::sys::pw_buffer>,
}

struct StreamBuffer {
    capture: DirectCaptureBuffer,
    pending_damage: DamageSet,
}

impl StreamingData {
    fn process(&mut self, stream: &pipewire::stream::Stream) {
        let buffer = unsafe { stream.dequeue_raw_buffer() };
        if buffer.is_null() {
            return;
        }

        let user_data = unsafe { (*buffer).user_data };
        if user_data.is_null() {
            error!("PipeWire buffer has no capture buffer attached");
            unsafe { stream.queue_raw_buffer(buffer) };
            return;
        }

        let stream_buffer = unsafe { &mut *(user_data as *mut StreamBuffer) };

        match self.capture.capture_into_buffer(
            &self.target,
            self.overlay_cursor,
            &stream_buffer.capture,
            &stream_buffer.pending_damage,
        ) {
            Ok(frame_damage) => {
                self.update_buffer_damage(buffer, &frame_damage);
            }
            Err(CaptureError::BufferConstraints) => {
                warn!("capture reported new buffer constraints; updating PipeWire params");
                match self.capture.probe(&self.target, self.overlay_cursor) {
                    Ok(probe) => {
                        match StreamFormatState::from_probe(probe, self.allow_shm_fallback) {
                            Ok(formats) => {
                                self.formats = formats;
                                self.reset_buffer_damage();
                                let format = format_param(
                                    self.formats.size.width,
                                    self.formats.size.height,
                                    &self.formats.spa_formats,
                                    &self.formats.modifiers,
                                );
                                let buffers = buffer_param(
                                    self.formats.size.width,
                                    self.formats.size.height,
                                    self.formats.dmabuf_supported,
                                    self.allow_shm_fallback,
                                );
                                let params = &mut [
                                    Pod::from_bytes(&format).expect("format pod must be valid"),
                                    Pod::from_bytes(&buffers).expect("buffer pod must be valid"),
                                ];

                                if let Err(err) = stream.update_params(params) {
                                    error!("failed to update PipeWire params: {err}");
                                }
                            }
                            Err(err) => error!("could not rebuild capture format state: {err}"),
                        }
                    }
                    Err(err) => error!("could not reprobe capture target: {err}"),
                }
            }
            Err(CaptureError::Stopped) => {
                error!("capture target stopped");
                let _ = stream.set_active(false);
            }
            Err(err) => error!("frame capture failed: {err}"),
        }

        unsafe { stream.queue_raw_buffer(buffer) };
    }

    fn add_buffer(&mut self, buffer: *mut pipewire::sys::pw_buffer) {
        let buf = unsafe { &mut *(*buffer).buffer };
        let datas = unsafe { slice::from_raw_parts_mut(buf.datas, buf.n_datas as usize) };
        let wants_dmabuf =
            !datas.is_empty() && (datas[0].type_ & (1 << spa_sys::SPA_DATA_DmaBuf) != 0);

        if wants_dmabuf && self.formats.dmabuf_supported {
            let selected_fourcc = self.chosen_format.and_then(spa_to_drm_fourcc);
            let selected_modifier = self
                .chosen_modifier
                .or_else(|| self.formats.modifiers.first().copied());
            let capture_buffer = match self
                .capture
                .create_dmabuf_buffer(&self.target, selected_fourcc, selected_modifier)
            {
                Ok(buffer) => buffer,
                Err(err) => {
                    error!("direct DMA-BUF capture buffer allocation failed: {err}");
                    return;
                }
            };

            let Some(bo) = capture_buffer.dmabuf_bo() else {
                error!("DMA-BUF capture allocation returned a non-DMA-BUF buffer");
                return;
            };
            let Some(format) = capture_buffer.dmabuf_format() else {
                error!("DMA-BUF capture buffer did not expose a format");
                return;
            };
            let Some(modifier) = capture_buffer.modifier() else {
                error!("DMA-BUF capture buffer did not expose a modifier");
                return;
            };
            let plane_count = bo.plane_count() as usize;

            info!(
                "allocating PipeWire DMA-BUF buffer: fourcc=0x{:08x}, size={}x{}, planes={}, modifier=0x{:016x}",
                format.fourcc,
                format.size.width,
                format.size.height,
                plane_count,
                modifier
            );

            for (index, data) in datas.iter_mut().take(plane_count).enumerate() {
                let fd = match bo.fd_for_plane(index as i32) {
                    Ok(fd) => fd,
                    Err(err) => {
                        error!("failed to export DMA-BUF plane {index}: {err}");
                        return;
                    }
                };
                let offset = bo.offset(index as i32);
                let stride = bo.stride_for_plane(index as i32);

                debug!(
                    "PipeWire DMA-BUF plane {index}: offset={offset}, stride={stride}, maxsize={}",
                    format.size.width * format.size.height * 4
                );

                data.type_ = spa_sys::SPA_DATA_DmaBuf;
                data.flags = 0;
                data.fd = fd.into_raw_fd().into();
                data.data = std::ptr::null_mut();
                data.maxsize = format.size.width * format.size.height * 4;
                data.mapoffset = 0;

                let chunk = unsafe { &mut *data.chunk };
                chunk.size = format.size.height * stride;
                chunk.offset = offset;
                chunk.stride = stride as i32;
            }

            self.attach_capture_buffer(buffer, capture_buffer);
            return;
        }

        if !self.allow_shm_fallback {
            error!(
                "PipeWire did not allocate DMA-BUF although SHM fallback is disabled; datas[0].type={:?}",
                datas.first().map(|data| data.type_)
            );
            return;
        }

        warn!("allocating PipeWire SHM buffer because DMA-BUF was not selected");
        if datas.len() != 1 {
            error!("expected one SHM PipeWire data block, got {}", datas.len());
            return;
        }
        let data = &mut datas[0];
        let size = self.formats.size;
        let name = c"xdg-desktop-portal-shiny";
        let fd = match rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC) {
            Ok(fd) => fd,
            Err(err) => {
                error!("failed to create memfd for SHM fallback: {err}");
                return;
            }
        };
        if let Err(err) = rustix::fs::ftruncate(&fd, (size.width * size.height * 4) as _) {
            error!("failed to resize memfd for SHM fallback: {err}");
            return;
        }
        let preferred_shm_format = self.chosen_format.and_then(spa_to_shm_format);
        let capture_buffer = match self
            .capture
            .create_shm_buffer(&self.target, preferred_shm_format, fd.as_fd())
        {
            Ok(buffer) => buffer,
            Err(err) => {
                error!("direct SHM capture buffer allocation failed: {err}");
                return;
            }
        };

        data.type_ = spa_sys::SPA_DATA_MemFd;
        data.flags = 0;
        data.fd = fd.into_raw_fd().into();
        data.data = std::ptr::null_mut();
        data.maxsize = size.width * size.height * 4;
        data.mapoffset = 0;

        let chunk = unsafe { &mut *data.chunk };
        chunk.size = size.width * size.height * 4;
        chunk.offset = 0;
        chunk.stride = 4 * size.width as i32;

        self.attach_capture_buffer(buffer, capture_buffer);
    }

    fn attach_capture_buffer(
        &mut self,
        buffer: *mut pipewire::sys::pw_buffer,
        capture: DirectCaptureBuffer,
    ) {
        unsafe {
            (*buffer).user_data = Box::into_raw(Box::new(StreamBuffer {
                capture,
                pending_damage: DamageSet::full(),
            })) as *mut c_void;
        }
        if !self.buffers.contains(&buffer) {
            self.buffers.push(buffer);
        }
    }

    fn remove_buffer(&mut self, buffer: *mut pipewire::sys::pw_buffer) {
        self.buffers.retain(|candidate| *candidate != buffer);

        let buf = unsafe { &mut *(*buffer).buffer };
        let datas = unsafe { slice::from_raw_parts_mut(buf.datas, buf.n_datas as usize) };

        for data in datas {
            if data.fd >= 0 {
                match data.fd.try_into() {
                    Ok(fd) => unsafe { rustix::io::close(fd) },
                    Err(err) => error!("invalid PipeWire buffer fd {}: {err}", data.fd),
                }
                data.fd = -1;
            }
        }

        if !unsafe { (*buffer).user_data }.is_null() {
            let stream_buffer: Box<StreamBuffer> =
                unsafe { Box::from_raw((*buffer).user_data as *mut _) };
            drop(stream_buffer);
            unsafe { (*buffer).user_data = std::ptr::null_mut() };
        }
    }

    fn update_buffer_damage(
        &mut self,
        current_buffer: *mut pipewire::sys::pw_buffer,
        frame_damage: &[DamageRect],
    ) {
        let mut propagated_buffers = 0;
        for buffer in &self.buffers {
            let user_data = unsafe { (**buffer).user_data };
            if user_data.is_null() {
                continue;
            }

            let stream_buffer = unsafe { &mut *(user_data as *mut StreamBuffer) };
            if *buffer == current_buffer {
                stream_buffer.pending_damage = DamageSet::empty();
            } else {
                stream_buffer
                    .pending_damage
                    .add_many(frame_damage, self.formats.size);
                propagated_buffers += 1;
            }
        }

        if !frame_damage.is_empty() {
            debug!(
                "captured frame reported {} damage rects; propagated to {} queued buffers",
                frame_damage.len(),
                propagated_buffers
            );
        }
    }

    fn reset_buffer_damage(&mut self) {
        for buffer in &self.buffers {
            let user_data = unsafe { (**buffer).user_data };
            if user_data.is_null() {
                continue;
            }

            let stream_buffer = unsafe { &mut *(user_data as *mut StreamBuffer) };
            stream_buffer.pending_damage = DamageSet::full();
        }
        debug!("reset pending damage to full for {} PipeWire buffers", self.buffers.len());
    }

    fn param_changed(&mut self, id: u32, pod: Option<&Pod>) {
        if id != spa_sys::SPA_PARAM_Format {
            return;
        }

        let Some(pod) = pod else {
            return;
        };

        let mut info = VideoInfoRaw::new();
        match info.parse(pod) {
            Ok(_) => {
                self.chosen_format = Some(info.format());
                self.chosen_modifier = Some(info.modifier());
                info!(
                    "PipeWire selected format={:?}, drm_fourcc={:?}, shm={:?}, modifier=0x{:016x}",
                    info.format(),
                    spa_to_drm_fourcc(info.format()).map(|fourcc| format!("0x{fourcc:08x}")),
                    spa_to_shm_format(info.format()),
                    info.modifier()
                );
            }
            Err(err) => error!("could not parse PipeWire format: {err}"),
        }
    }
}

type PipewireStreamResult = (
    pipewire::main_loop::MainLoopRc,
    pipewire::stream::StreamListener<StreamingData>,
    pipewire::stream::StreamRc,
    pipewire::context::ContextRc,
    oneshot::Receiver<anyhow::Result<(u32, Size)>>,
);

fn start_stream(
    mut capture: DirectCapture,
    overlay_cursor: bool,
    target: CaptureTarget,
) -> anyhow::Result<PipewireStreamResult> {
    let main_loop = pipewire::main_loop::MainLoopRc::new(None)?;
    let context = pipewire::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;

    let stream = pipewire::stream::StreamRc::new(
        core,
        "xdg-desktop-portal-shiny",
        pipewire::properties::properties! {
            "media.class" => "Video/Source",
            "node.name" => "xdg-desktop-portal-shiny",
            "node.description" => "Shiny screen capture",
        },
    )?;

    let allow_shm_fallback = std::env::var_os("SHINY_PORTAL_ALLOW_SHM").is_some();
    if allow_shm_fallback {
        warn!("SHINY_PORTAL_ALLOW_SHM is set; SHM fallback is enabled");
    }

    let probe = capture.probe(&target, overlay_cursor)?;
    log_probe(&probe);
    let formats = StreamFormatState::from_probe(probe, allow_shm_fallback)?;

    info!(
        "advertising PipeWire formats {:?} at {}x{} with DMA-BUF modifiers {:?}",
        formats.spa_formats, formats.size.width, formats.size.height, formats.modifiers
    );

    let (node_id_tx, node_id_rx) = oneshot::channel();
    let mut node_id_tx = Some(node_id_tx);
    let stream_size = formats.size;

    let listener = stream
        .add_local_listener_with_user_data(StreamingData {
            capture,
            target,
            overlay_cursor,
            formats: formats.clone(),
            chosen_format: None,
            chosen_modifier: None,
            allow_shm_fallback,
            buffers: Vec::new(),
        })
        .state_changed(move |stream, _, old, new| {
            info!("PipeWire stream state changed: {old:?} -> {new:?}");
            match new {
                StreamState::Paused => {
                    if let Some(tx) = node_id_tx.take() {
                        let _ = tx.send(Ok((stream.node_id(), stream_size)));
                    }
                }
                StreamState::Error(err) => error!("PipeWire stream error: {err}"),
                _ => {}
            }
        })
        .param_changed(|_, streaming_data, id, pod| {
            streaming_data.param_changed(id, pod);
        })
        .add_buffer(|_, streaming_data, buffer| {
            streaming_data.add_buffer(buffer);
        })
        .remove_buffer(|_, streaming_data, buffer| {
            streaming_data.remove_buffer(buffer);
        })
        .process(|stream, streaming_data| {
            streaming_data.process(stream);
        })
        .register()?;

    let format = format_param(
        formats.size.width,
        formats.size.height,
        &formats.spa_formats,
        &formats.modifiers,
    );
    let buffers = buffer_param(
        formats.size.width,
        formats.size.height,
        formats.dmabuf_supported,
        allow_shm_fallback,
    );
    let params = &mut [
        Pod::from_bytes(&format).expect("format pod must be valid"),
        Pod::from_bytes(&buffers).expect("buffer pod must be valid"),
    ];

    stream.connect(
        spa::utils::Direction::Output,
        None,
        pipewire::stream::StreamFlags::ALLOC_BUFFERS,
        params,
    )?;

    Ok((main_loop, listener, stream, context, node_id_rx))
}

fn log_probe(probe: &CaptureProbe) {
    for format in &probe.dmabuf_formats {
        debug!(
            "Wayland DMA-BUF format: fourcc=0x{:08x}, modifiers={:?}, size={}x{}",
            format.fourcc, format.modifiers, format.size.width, format.size.height
        );
    }
    for format in &probe.shm_formats {
        debug!(
            "Wayland SHM format: {:?}, size={}x{}, stride={}",
            format.format, format.size.width, format.size.height, format.stride
        );
    }
}

fn value_to_bytes(value: pod::Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut cursor = io::Cursor::new(&mut bytes);
    PodSerializer::serialize(&mut cursor, &value).expect("serialize pod value");
    bytes
}

fn buffer_param(
    width: u32,
    height: u32,
    dmabuf_supported: bool,
    allow_shm_fallback: bool,
) -> Vec<u8> {
    let dmabuf_bit = 1 << spa_sys::SPA_DATA_DmaBuf;
    let memfd_bit = 1 << spa_sys::SPA_DATA_MemFd;
    let data_type_flags = if dmabuf_supported {
        if allow_shm_fallback {
            vec![dmabuf_bit, memfd_bit]
        } else {
            vec![dmabuf_bit]
        }
    } else {
        vec![memfd_bit]
    };
    let data_type_default = *data_type_flags.first().expect("buffer data type flags");

    info!(
        "advertising PipeWire buffer data types: default={}, flags={:?}",
        data_type_default, data_type_flags
    );

    value_to_bytes(pod::Value::Object(pod::Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa_sys::SPA_PARAM_Buffers,
        properties: vec![
            pod::Property {
                key: spa_sys::SPA_PARAM_BUFFERS_dataType,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Flags {
                        default: data_type_default as i32,
                        flags: data_type_flags
                            .into_iter()
                            .map(|flag| flag as i32)
                            .collect(),
                    },
                ))),
            },
            pod::Property {
                key: spa_sys::SPA_PARAM_BUFFERS_size,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(width as i32 * height as i32 * 4),
            },
            pod::Property {
                key: spa_sys::SPA_PARAM_BUFFERS_stride,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(width as i32 * 4),
            },
            pod::Property {
                key: spa_sys::SPA_PARAM_BUFFERS_align,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(16),
            },
            pod::Property {
                key: spa_sys::SPA_PARAM_BUFFERS_blocks,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(1),
            },
            pod::Property {
                key: spa_sys::SPA_PARAM_BUFFERS_buffers,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Range {
                        default: 4,
                        min: 1,
                        max: 32,
                    },
                ))),
            },
        ],
    }))
}

fn format_param(
    width: u32,
    height: u32,
    available_video_formats: &[VideoFormat],
    dmabuf_modifiers: &[u64],
) -> Vec<u8> {
    let mut obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width, height },
            spa::utils::Rectangle { width, height },
            spa::utils::Rectangle { width, height }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 1, denom: 1 },
            spa::utils::Fraction { num: 60, denom: 1 }
        )
    );

    obj.properties.push(pod::Property {
        key: FormatProperties::VideoFormat.as_raw(),
        flags: pod::PropertyFlags::empty(),
        value: pod::Value::Choice(pod::ChoiceValue::Id(spa::utils::Choice(
            spa::utils::ChoiceFlags::empty(),
            spa::utils::ChoiceEnum::Enum {
                default: spa::utils::Id(available_video_formats[0].as_raw()),
                alternatives: available_video_formats
                    .iter()
                    .map(|format| spa::utils::Id(format.as_raw()))
                    .collect(),
            },
        ))),
    });

    if !dmabuf_modifiers.is_empty() {
        obj.properties.push(pod::Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Choice(pod::ChoiceValue::Long(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Enum {
                    default: dmabuf_modifiers[0] as i64,
                    alternatives: dmabuf_modifiers
                        .iter()
                        .map(|modifier| *modifier as i64)
                        .collect(),
                },
            ))),
        });
    }

    value_to_bytes(pod::Value::Object(obj))
}

fn preferred_dmabuf_formats(formats: &[DmabufFormat]) -> Vec<DmabufFormat> {
    const PREFERRED: &[u32] = &[
        fourcc("XR24"),
        fourcc("AR24"),
        fourcc("XB24"),
        fourcc("AB24"),
        fourcc("XR30"),
        fourcc("AR30"),
        fourcc("XB30"),
        fourcc("AB30"),
    ];

    let mut result = Vec::new();
    for fourcc in PREFERRED {
        for format in formats.iter().filter(|format| format.fourcc == *fourcc) {
            if !result.contains(format) {
                result.push(format.clone());
            }
        }
    }
    for format in formats {
        if !result.contains(format) {
            result.push(format.clone());
        }
    }
    result
}

fn preferred_modifiers(modifiers: &[u64]) -> Vec<u64> {
    let mut result = Vec::new();
    for modifier in [DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_MOD_INVALID] {
        if modifiers.contains(&modifier) {
            result.push(modifier);
        }
    }
    for modifier in modifiers {
        if !result.contains(modifier) {
            result.push(*modifier);
        }
    }
    result
}

fn drm_fourcc_to_spa(fourcc_code: u32) -> Option<VideoFormat> {
    match fourcc_code {
        code if code == fourcc("AR24") => Some(VideoFormat::BGRA),
        code if code == fourcc("XR24") => Some(VideoFormat::BGRx),
        code if code == fourcc("RA24") => Some(VideoFormat::ABGR),
        code if code == fourcc("RX24") => Some(VideoFormat::xBGR),
        code if code == fourcc("AB24") => Some(VideoFormat::RGBA),
        code if code == fourcc("XB24") => Some(VideoFormat::RGBx),
        code if code == fourcc("BA24") => Some(VideoFormat::ARGB),
        code if code == fourcc("BX24") => Some(VideoFormat::xRGB),
        code if code == fourcc("XR30") => Some(VideoFormat::xRGB_210LE),
        code if code == fourcc("XB30") => Some(VideoFormat::xBGR_210LE),
        code if code == fourcc("AR30") => Some(VideoFormat::ARGB_210LE),
        code if code == fourcc("AB30") => Some(VideoFormat::ABGR_210LE),
        _ => None,
    }
}

fn spa_to_drm_fourcc(format: VideoFormat) -> Option<u32> {
    match format {
        VideoFormat::BGRA => Some(fourcc("AR24")),
        VideoFormat::BGRx => Some(fourcc("XR24")),
        VideoFormat::ABGR => Some(fourcc("RA24")),
        VideoFormat::xBGR => Some(fourcc("RX24")),
        VideoFormat::RGBA => Some(fourcc("AB24")),
        VideoFormat::RGBx => Some(fourcc("XB24")),
        VideoFormat::ARGB => Some(fourcc("BA24")),
        VideoFormat::xRGB => Some(fourcc("BX24")),
        VideoFormat::xRGB_210LE => Some(fourcc("XR30")),
        VideoFormat::xBGR_210LE => Some(fourcc("XB30")),
        VideoFormat::ARGB_210LE => Some(fourcc("AR30")),
        VideoFormat::ABGR_210LE => Some(fourcc("AB30")),
        _ => None,
    }
}

fn shm_format_to_spa(format: wl_shm::Format) -> Option<VideoFormat> {
    match format {
        wl_shm::Format::Argb8888 => Some(VideoFormat::BGRA),
        wl_shm::Format::Xrgb8888 => Some(VideoFormat::BGRx),
        wl_shm::Format::Rgba8888 => Some(VideoFormat::ABGR),
        wl_shm::Format::Rgbx8888 => Some(VideoFormat::xBGR),
        wl_shm::Format::Abgr8888 => Some(VideoFormat::RGBA),
        wl_shm::Format::Xbgr8888 => Some(VideoFormat::RGBx),
        wl_shm::Format::Bgra8888 => Some(VideoFormat::ARGB),
        wl_shm::Format::Bgrx8888 => Some(VideoFormat::xRGB),
        wl_shm::Format::Xrgb2101010 => Some(VideoFormat::xRGB_210LE),
        wl_shm::Format::Xbgr2101010 => Some(VideoFormat::xBGR_210LE),
        wl_shm::Format::Argb2101010 => Some(VideoFormat::ARGB_210LE),
        wl_shm::Format::Abgr2101010 => Some(VideoFormat::ABGR_210LE),
        _ => None,
    }
}

fn spa_to_shm_format(format: VideoFormat) -> Option<wl_shm::Format> {
    match format {
        VideoFormat::BGRA => Some(wl_shm::Format::Argb8888),
        VideoFormat::BGRx => Some(wl_shm::Format::Xrgb8888),
        VideoFormat::ABGR => Some(wl_shm::Format::Rgba8888),
        VideoFormat::xBGR => Some(wl_shm::Format::Rgbx8888),
        VideoFormat::RGBA => Some(wl_shm::Format::Abgr8888),
        VideoFormat::RGBx => Some(wl_shm::Format::Xbgr8888),
        VideoFormat::ARGB => Some(wl_shm::Format::Bgra8888),
        VideoFormat::xRGB => Some(wl_shm::Format::Bgrx8888),
        VideoFormat::xRGB_210LE => Some(wl_shm::Format::Xrgb2101010),
        VideoFormat::xBGR_210LE => Some(wl_shm::Format::Xbgr2101010),
        VideoFormat::ARGB_210LE => Some(wl_shm::Format::Argb2101010),
        VideoFormat::ABGR_210LE => Some(wl_shm::Format::Abgr2101010),
        _ => None,
    }
}
