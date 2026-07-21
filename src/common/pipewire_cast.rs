use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    io,
    os::fd::{AsFd, IntoRawFd},
    rc::Rc,
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
        pod::{self, Pod, deserialize::PodDeserializer, serialize::PodSerializer},
        sys as spa_sys,
    },
    stream::StreamState,
    types::ObjectType,
};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use wayland_client::protocol::wl_shm;

use crate::{
    common::wayland_capture::{
        CaptureError, CaptureProbe, CaptureTarget, DamageRect, DamageSet, DirectCapture,
        DirectCaptureBuffer, Size, fourcc,
    },
    config::Config,
};

pub struct ScreencastThread {
    node_id: u32,
    pipewire_serial: u64,
    size: Size,
    thread_stop_tx: pipewire::channel::Sender<()>,
}

impl ScreencastThread {
    pub async fn start_cast(
        overlay_cursor: bool,
        target: CaptureTarget,
        capture: DirectCapture,
        config: Config,
    ) -> anyhow::Result<Self> {
        let (node_id_tx, node_id_rx) = oneshot::channel();
        let (thread_stop_tx, thread_stop_rx) = pipewire::channel::channel::<()>();

        std::thread::spawn(
            move || match start_stream(capture, overlay_cursor, target, config) {
                Ok((
                    main_loop,
                    listener,
                    _stream,
                    _registry,
                    registry_listener,
                    context,
                    node_id_ready,
                )) => {
                    let _ = node_id_tx.send(Ok(node_id_ready));
                    let weak_loop = main_loop.downgrade();

                    let _receiver = thread_stop_rx.attach(main_loop.loop_(), move |()| {
                        if let Some(main_loop) = weak_loop.upgrade() {
                            main_loop.quit();
                        }
                    });

                    main_loop.run();
                    drop(listener);
                    drop(registry_listener);
                    drop(context);
                }
                Err(err) => {
                    let _ = node_id_tx.send(Err(err));
                }
            },
        );

        let (node_id, pipewire_serial, size) = node_id_rx.await??.await??;

        Ok(Self {
            node_id,
            pipewire_serial,
            size,
            thread_stop_tx,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    pub fn pipewire_serial(&self) -> u64 {
        self.pipewire_serial
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
    offers: Vec<StreamFormatOffer>,
    dmabuf_supported: bool,
}

#[derive(Debug, Clone)]
struct StreamFormatOffer {
    spa_format: VideoFormat,
    modifiers: Vec<u64>,
}

impl StreamFormatState {
    fn from_probe(probe: CaptureProbe, allow_shm_fallback: bool) -> anyhow::Result<Self> {
        let dmabuf_available = probe.has_dmabuf_device && !probe.dmabuf_formats.is_empty();
        if !dmabuf_available && !allow_shm_fallback {
            anyhow::bail!("compositor did not advertise a usable dma-buf capture path");
        }

        let mut offers = Vec::new();

        if dmabuf_available {
            for format in &probe.dmabuf_formats {
                let Some(spa_format) = drm_fourcc_to_spa(format.fourcc) else {
                    continue;
                };

                if format.modifiers.is_empty() {
                    continue;
                }

                offers.push(StreamFormatOffer {
                    spa_format,
                    modifiers: format.modifiers.clone(),
                });
            }
        }

        let dmabuf_supported = !offers.is_empty();
        if !dmabuf_supported && !allow_shm_fallback {
            anyhow::bail!("compositor did not advertise usable dma-buf formats and modifiers");
        }

        if allow_shm_fallback {
            for format in &probe.shm_formats {
                if let Some(spa_format) = shm_format_to_spa(format.format)
                    && !offers
                        .iter()
                        .any(|offer| offer.spa_format == spa_format && offer.modifiers.is_empty())
                {
                    offers.push(StreamFormatOffer {
                        spa_format,
                        modifiers: Vec::new(),
                    });
                }
            }
        }

        if offers.is_empty() {
            anyhow::bail!("no capture formats could be mapped to pw spa video formats");
        }

        Ok(Self {
            size: probe.size,
            offers,
            dmabuf_supported,
        })
    }

    fn format_params(&self, max_fps: u32) -> Vec<Vec<u8>> {
        self.offers
            .iter()
            .map(|offer| {
                format_param(
                    self.size.width,
                    self.size.height,
                    offer.spa_format,
                    &offer.modifiers,
                    max_fps,
                )
            })
            .collect()
    }

    fn fallback_modifier(&self, format: VideoFormat) -> Option<u64> {
        self.offers
            .iter()
            .find(|offer| offer.spa_format == format)
            .and_then(|offer| offer.modifiers.first().copied())
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
    max_fps: u32,
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
            error!("pw buffer has no capture buffer attached");
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
                warn!("capture reported new buffer constraints; updating pw params");
                match self.capture.probe(&self.target, self.overlay_cursor) {
                    Ok(probe) => {
                        match StreamFormatState::from_probe(probe, self.allow_shm_fallback) {
                            Ok(formats) => {
                                self.formats = formats;
                                self.reset_buffer_damage();

                                let formats = self.formats.format_params(self.max_fps);
                                let mut params = formats
                                    .iter()
                                    .map(|format| {
                                        Pod::from_bytes(format).expect("format pod must be valid")
                                    })
                                    .collect::<Vec<_>>();

                                if let Err(err) = stream.update_params(&mut params) {
                                    error!("failed to update pw params: {err}");
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
            let selected_modifier = self.chosen_modifier.or_else(|| {
                self.chosen_format
                    .and_then(|format| self.formats.fallback_modifier(format))
            });

            let capture_buffer = match self.capture.create_dmabuf_buffer(
                &self.target,
                selected_fourcc,
                selected_modifier,
            ) {
                Ok(buffer) => buffer,
                Err(err) => {
                    error!("direct dma-buf capture buffer allocation failed: {err}");
                    return;
                }
            };

            let Some(bo) = capture_buffer.dmabuf_bo() else {
                error!("dma-buf capture allocation returned a non-dma-buf buffer");
                return;
            };

            let Some(format) = capture_buffer.dmabuf_format() else {
                error!("dma-buf capture buffer did not expose a format");
                return;
            };

            let Some(modifier) = capture_buffer.modifier() else {
                error!("dma-buf capture buffer did not expose a modifier");
                return;
            };

            let plane_count = bo.plane_count() as usize;
            if datas.len() != plane_count {
                error!(
                    "pw allocated {} dma-buf data blocks, but gbm buffer has {} planes",
                    datas.len(),
                    plane_count
                );
                return;
            }

            info!(
                "allocating pw dma-buf buffer: fourcc=0x{:08x}, size={}x{}, planes={}, modifier=0x{:016x}",
                format.fourcc, format.size.width, format.size.height, plane_count, modifier
            );

            for (index, data) in datas.iter_mut().enumerate() {
                let fd = match bo.fd_for_plane(index as i32) {
                    Ok(fd) => fd,
                    Err(err) => {
                        error!("failed to export dma-buf plane {index}: {err}");
                        return;
                    }
                };

                let offset = bo.offset(index as i32);
                let stride = bo.stride_for_plane(index as i32);

                debug!(
                    "pw dma-buf plane {index}: offset={offset}, stride={stride}, maxsize={}",
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
                "pw did not allocate dma-buf although shm fallback is disabled; datas[0].type={:?}",
                datas.first().map(|data| data.type_)
            );
            return;
        }

        warn!("allocating pw shm buffer because dma-buf was not selected");
        if datas.len() != 1 {
            error!("expected one shm pw data block, got {}", datas.len());
            return;
        }

        let data = &mut datas[0];
        let size = self.formats.size;
        let name = c"xdg-desktop-portal-shiny";
        let fd = match rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC) {
            Ok(fd) => fd,
            Err(err) => {
                error!("failed to create memfd for shm fallback: {err}");
                return;
            }
        };

        if let Err(err) = rustix::fs::ftruncate(&fd, (size.width * size.height * 4) as _) {
            error!("failed to resize memfd for shm fallback: {err}");
            return;
        }

        let preferred_shm_format = self.chosen_format.and_then(spa_to_shm_format);
        let capture_buffer =
            match self
                .capture
                .create_shm_buffer(&self.target, preferred_shm_format, fd.as_fd())
            {
                Ok(buffer) => buffer,
                Err(err) => {
                    error!("direct shm capture buffer allocation failed: {err}");
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
                    Err(err) => error!("invalid pw buffer fd {}: {err}", data.fd),
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

        debug!(
            "reset pending damage to full for {} pw buffers",
            self.buffers.len()
        );
    }

    fn param_changed(&mut self, stream: &pipewire::stream::Stream, id: u32, pod: Option<&Pod>) {
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
                let modifier_flags = pod_modifier_flags(pod);
                let uses_dmabuf = modifier_flags.is_some();
                if modifier_flags
                    .is_some_and(|flags| flags.contains(pod::PropertyFlags::DONT_FIXATE))
                {
                    let fixed_format = fixed_format_param(
                        self.formats.size.width,
                        self.formats.size.height,
                        info.format(),
                        info.modifier(),
                        self.max_fps,
                    );

                    let format_params = self.formats.format_params(self.max_fps);
                    let mut params = vec![
                        Pod::from_bytes(&fixed_format).expect("fixed format pod must be valid"),
                    ];

                    params.extend(
                        format_params.iter().map(|format| {
                            Pod::from_bytes(format).expect("format pod must be valid")
                        }),
                    );

                    info!(
                        "fixating pw dma-buf format={:?}, modifier=0x{:016x}",
                        info.format(),
                        info.modifier()
                    );

                    if let Err(err) = stream.update_params(&mut params) {
                        error!("failed to fixate selected pw format: {err}");
                    }

                    return;
                }

                let plane_count = if uses_dmabuf {
                    self.chosen_modifier = Some(info.modifier());
                    let Some(fourcc) = spa_to_drm_fourcc(info.format()) else {
                        error!(
                            "pw selected format {:?}, which has no drm fourcc mapping",
                            info.format()
                        );
                        return;
                    };

                    match self.capture.dmabuf_plane_count(fourcc, info.modifier()) {
                        Ok(plane_count) if plane_count > 0 => plane_count,
                        Ok(_) => {
                            error!("gbm returned zero dma-buf planes for selected format");
                            return;
                        }
                        Err(err) => {
                            error!("failed to determine selected dma-buf plane count: {err}");
                            return;
                        }
                    }
                } else {
                    self.chosen_modifier = None;
                    1
                };

                let framerate = info.framerate();
                let max_framerate = info.max_framerate();

                info!(
                    "pw selected format={:?}, drm_fourcc={:?}, shm={:?}, buffer_type={}, modifier=0x{:016x}, planes={}, framerate={}/{}, max_framerate={}/{}",
                    info.format(),
                    spa_to_drm_fourcc(info.format()).map(|fourcc| format!("0x{fourcc:08x}")),
                    spa_to_shm_format(info.format()),
                    if uses_dmabuf { "dma-buf" } else { "shm" },
                    info.modifier(),
                    plane_count,
                    framerate.num,
                    framerate.denom,
                    max_framerate.num,
                    max_framerate.denom
                );

                let buffers = buffer_param(
                    self.formats.size.width,
                    self.formats.size.height,
                    plane_count,
                    uses_dmabuf,
                );

                let mut params = [Pod::from_bytes(&buffers).expect("buffer pod must be valid")];
                if let Err(err) = stream.update_params(&mut params) {
                    error!("failed to publish selected pw buffer parameters: {err}");
                }
            }
            Err(err) => error!("could not parse pw format: {err}"),
        }
    }
}

type PipewireStreamResult = (
    pipewire::main_loop::MainLoopRc,
    pipewire::stream::StreamListener<StreamingData>,
    pipewire::stream::StreamRc,
    pipewire::registry::RegistryRc,
    pipewire::registry::Listener,
    pipewire::context::ContextRc,
    oneshot::Receiver<anyhow::Result<(u32, u64, Size)>>,
);

struct StreamReadyState {
    sender: Option<oneshot::Sender<anyhow::Result<(u32, u64, Size)>>>,
    node_id: Option<u32>,
    serials: HashMap<u32, u64>,
    size: Size,
}

impl StreamReadyState {
    fn set_node(&mut self, node_id: u32, serial: Option<u64>) {
        self.node_id = Some(node_id);
        if let Some(serial) = serial {
            self.serials.insert(node_id, serial);
        }

        self.send_if_ready();
    }

    fn set_serial(&mut self, node_id: u32, serial: u64) {
        self.serials.insert(node_id, serial);
        self.send_if_ready();
    }

    fn send_if_ready(&mut self) {
        let Some(node_id) = self.node_id else {
            return;
        };

        let Some(serial) = self.serials.get(&node_id).copied() else {
            return;
        };

        let Some(sender) = self.sender.take() else {
            return;
        };

        let _ = sender.send(Ok((node_id, serial, self.size)));
    }
}

fn start_stream(
    mut capture: DirectCapture,
    overlay_cursor: bool,
    target: CaptureTarget,
    config: Config,
) -> anyhow::Result<PipewireStreamResult> {
    let main_loop = pipewire::main_loop::MainLoopRc::new(None)?;
    let context = pipewire::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let stream = pipewire::stream::StreamRc::new(
        core.clone(),
        "xdg-desktop-portal-shiny",
        pipewire::properties::properties! {
            "media.class" => "Video/Source",
            "node.name" => "xdg-desktop-portal-shiny",
            "node.description" => "Shiny screen capture",
        },
    )?;

    let allow_shm_fallback = config.allow_screencast_shm;
    if allow_shm_fallback {
        warn!("shm fallback is enabled by configuration");
    }

    if config.max_fps == 0 {
        info!("screencast capture rate is unlimited");
    } else {
        info!("limiting screencast capture rate to {} fps", config.max_fps);
    }

    let probe = capture.probe(&target, overlay_cursor)?;

    for format in &probe.dmabuf_formats {
        debug!(
            "wayland dma-buf format: fourcc=0x{:08x}, modifiers={:?}, size={}x{}",
            format.fourcc, format.modifiers, format.size.width, format.size.height
        );
    }

    for format in &probe.shm_formats {
        debug!(
            "wayland shm format: {:?}, size={}x{}, stride={}",
            format.format, format.size.width, format.size.height, format.stride
        );
    }

    let formats = StreamFormatState::from_probe(probe, allow_shm_fallback)?;

    for offer in &formats.offers {
        info!(
            "advertising pw format {:?} at {}x{} with dma-buf modifiers {:?}",
            offer.spa_format, formats.size.width, formats.size.height, offer.modifiers
        );
    }

    let (node_id_tx, node_id_rx) = oneshot::channel();
    let stream_size = formats.size;
    let ready_state = Rc::new(RefCell::new(StreamReadyState {
        sender: Some(node_id_tx),
        node_id: None,
        serials: HashMap::new(),
        size: stream_size,
    }));

    let registry_ready_state = ready_state.clone();
    let registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != ObjectType::Node {
                return;
            }

            let Some(serial) = global
                .props
                .as_ref()
                .and_then(|props| props.get(&pipewire::keys::OBJECT_SERIAL))
                .and_then(|serial| serial.parse::<u64>().ok())
            else {
                return;
            };

            registry_ready_state
                .borrow_mut()
                .set_serial(global.id, serial);
        })
        .register();

    let stream_ready_state = ready_state.clone();
    let listener = stream
        .add_local_listener_with_user_data(StreamingData {
            capture,
            target,
            overlay_cursor,
            formats: formats.clone(),
            chosen_format: None,
            chosen_modifier: None,
            allow_shm_fallback,
            max_fps: config.max_fps,
            buffers: Vec::new(),
        })
        .state_changed(move |stream, _, old, new| {
            info!("pw stream state changed: {old:?} -> {new:?}");
            match new {
                StreamState::Paused => {
                    let serial = stream
                        .properties()
                        .get(&pipewire::keys::OBJECT_SERIAL)
                        .and_then(|serial| serial.parse::<u64>().ok());
                    stream_ready_state
                        .borrow_mut()
                        .set_node(stream.node_id(), serial);
                }
                StreamState::Error(err) => error!("pw stream error: {err}"),
                _ => {}
            }
        })
        .param_changed(|stream, streaming_data, id, pod| {
            streaming_data.param_changed(stream, id, pod);
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

    let format_params = formats.format_params(config.max_fps);
    let mut params = format_params
        .iter()
        .map(|format| Pod::from_bytes(format).expect("format pod must be valid"))
        .collect::<Vec<_>>();

    stream.connect(
        spa::utils::Direction::Output,
        None,
        pipewire::stream::StreamFlags::ALLOC_BUFFERS,
        &mut params,
    )?;

    Ok((
        main_loop,
        listener,
        stream,
        registry,
        registry_listener,
        context,
        node_id_rx,
    ))
}

fn value_to_bytes(value: pod::Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut cursor = io::Cursor::new(&mut bytes);
    PodSerializer::serialize(&mut cursor, &value).expect("serialize pod value");
    bytes
}

fn pod_modifier_flags(pod: &Pod) -> Option<pod::PropertyFlags> {
    PodDeserializer::deserialize_from::<pod::Value>(pod.as_bytes())
        .ok()
        .and_then(|(_, value)| {
            let pod::Value::Object(object) = value else {
                return None;
            };
            object
                .properties
                .iter()
                .find(|property| property.key == FormatProperties::VideoModifier.as_raw())
                .map(|property| property.flags)
        })
}

fn buffer_param(width: u32, height: u32, blocks: u32, use_dmabuf: bool) -> Vec<u8> {
    let dmabuf_bit = 1 << spa_sys::SPA_DATA_DmaBuf;
    let memfd_bit = 1 << spa_sys::SPA_DATA_MemFd;
    let data_type = if use_dmabuf { dmabuf_bit } else { memfd_bit };

    info!(
        "advertising pw buffer data type={} with {} blocks",
        data_type, blocks
    );

    let mut properties = vec![
        pod::Property {
            key: spa_sys::SPA_PARAM_BUFFERS_dataType,
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Choice(pod::ChoiceValue::Int(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Flags {
                    default: data_type,
                    flags: vec![data_type],
                },
            ))),
        },
        pod::Property {
            key: spa_sys::SPA_PARAM_BUFFERS_align,
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Int(16),
        },
        pod::Property {
            key: spa_sys::SPA_PARAM_BUFFERS_blocks,
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Int(blocks as i32),
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
    ];

    properties.extend([
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
    ]);

    value_to_bytes(pod::Value::Object(pod::Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa_sys::SPA_PARAM_Buffers,
        properties,
    }))
}

fn format_param(
    width: u32,
    height: u32,
    video_format: VideoFormat,
    dmabuf_modifiers: &[u64],
    max_fps: u32,
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
        )
    );

    obj.properties.push(pod::Property {
        key: FormatProperties::VideoFramerate.as_raw(),
        flags: pod::PropertyFlags::empty(),
        value: pod::Value::Fraction(spa::utils::Fraction { num: 0, denom: 1 }),
    });

    if max_fps > 0 {
        obj.properties.push(pod::Property {
            key: FormatProperties::VideoMaxFramerate.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Choice(pod::ChoiceValue::Fraction(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Range {
                    default: spa::utils::Fraction {
                        num: max_fps,
                        denom: 1,
                    },
                    min: spa::utils::Fraction { num: 1, denom: 1 },
                    max: spa::utils::Fraction {
                        num: max_fps,
                        denom: 1,
                    },
                },
            ))),
        });
    }

    obj.properties.push(pod::Property {
        key: FormatProperties::VideoFormat.as_raw(),
        flags: pod::PropertyFlags::empty(),
        value: pod::Value::Id(spa::utils::Id(video_format.as_raw())),
    });

    if !dmabuf_modifiers.is_empty() {
        obj.properties.push(pod::Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: pod::PropertyFlags::MANDATORY | pod::PropertyFlags::DONT_FIXATE,
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

fn fixed_format_param(
    width: u32,
    height: u32,
    video_format: VideoFormat,
    modifier: u64,
    max_fps: u32,
) -> Vec<u8> {
    let mut obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle { width, height }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction { num: 0, denom: 1 }
        )
    );

    obj.properties.push(pod::Property {
        key: FormatProperties::VideoFormat.as_raw(),
        flags: pod::PropertyFlags::empty(),
        value: pod::Value::Id(spa::utils::Id(video_format.as_raw())),
    });

    obj.properties.push(pod::Property {
        key: FormatProperties::VideoModifier.as_raw(),
        flags: pod::PropertyFlags::MANDATORY,
        value: pod::Value::Long(modifier as i64),
    });

    if max_fps > 0 {
        obj.properties.push(pod::Property {
            key: FormatProperties::VideoMaxFramerate.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Choice(pod::ChoiceValue::Fraction(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Range {
                    default: spa::utils::Fraction {
                        num: max_fps,
                        denom: 1,
                    },
                    min: spa::utils::Fraction { num: 1, denom: 1 },
                    max: spa::utils::Fraction {
                        num: max_fps,
                        denom: 1,
                    },
                },
            ))),
        });
    }

    value_to_bytes(pod::Value::Object(obj))
}

fn drm_fourcc_to_spa(fourcc_code: u32) -> Option<VideoFormat> {
    match fourcc_code {
        code if code == fourcc(b"AR24") => Some(VideoFormat::BGRA),
        code if code == fourcc(b"XR24") => Some(VideoFormat::BGRx),
        code if code == fourcc(b"RA24") => Some(VideoFormat::ABGR),
        code if code == fourcc(b"RX24") => Some(VideoFormat::xBGR),
        code if code == fourcc(b"AB24") => Some(VideoFormat::RGBA),
        code if code == fourcc(b"XB24") => Some(VideoFormat::RGBx),
        code if code == fourcc(b"BA24") => Some(VideoFormat::ARGB),
        code if code == fourcc(b"BX24") => Some(VideoFormat::xRGB),
        code if code == fourcc(b"XR30") => Some(VideoFormat::xRGB_210LE),
        code if code == fourcc(b"XB30") => Some(VideoFormat::xBGR_210LE),
        code if code == fourcc(b"AR30") => Some(VideoFormat::ARGB_210LE),
        code if code == fourcc(b"AB30") => Some(VideoFormat::ABGR_210LE),
        _ => None,
    }
}

fn spa_to_drm_fourcc(format: VideoFormat) -> Option<u32> {
    match format {
        VideoFormat::BGRA => Some(fourcc(b"AR24")),
        VideoFormat::BGRx => Some(fourcc(b"XR24")),
        VideoFormat::ABGR => Some(fourcc(b"RA24")),
        VideoFormat::xBGR => Some(fourcc(b"RX24")),
        VideoFormat::RGBA => Some(fourcc(b"AB24")),
        VideoFormat::RGBx => Some(fourcc(b"XB24")),
        VideoFormat::ARGB => Some(fourcc(b"BA24")),
        VideoFormat::xRGB => Some(fourcc(b"BX24")),
        VideoFormat::xRGB_210LE => Some(fourcc(b"XR30")),
        VideoFormat::xBGR_210LE => Some(fourcc(b"XB30")),
        VideoFormat::ARGB_210LE => Some(fourcc(b"AR30")),
        VideoFormat::ABGR_210LE => Some(fourcc(b"AB30")),
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
