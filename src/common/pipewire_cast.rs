use std::{ffi::c_void, io, os::fd::IntoRawFd, slice};

use libwayshot::{
    WayshotConnection, WayshotTarget,
    reexport::{ExtForeignToplevelHandleV1, FailureReason, WlOutput},
    region::EmbeddedRegion,
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
use wayland_client::{WEnum, protocol::wl_shm::Format};

const LINEAR_MODIFIER: i64 = 0;

pub struct ScreencastThread {
    node_id: u32,
    thread_stop_tx: pipewire::channel::Sender<()>,
}

#[derive(Debug, Clone)]
pub enum CastTarget {
    TopLevel(ExtForeignToplevelHandleV1),
    Screen(WlOutput),
}

impl CastTarget {
    fn wayshot_target(&self) -> WayshotTarget {
        match self {
            Self::Screen(screen) => WayshotTarget::Screen(screen.clone()),
            Self::TopLevel(toplevel) => WayshotTarget::Toplevel(toplevel.clone()),
        }
    }
}

impl ScreencastThread {
    pub async fn start_cast(
        overlay_cursor: bool,
        embedded_region: Option<EmbeddedRegion>,
        target: CastTarget,
        connection: WayshotConnection,
    ) -> anyhow::Result<Self> {
        let (node_id_tx, node_id_rx) = oneshot::channel();
        let (thread_stop_tx, thread_stop_rx) = pipewire::channel::channel::<()>();

        std::thread::spawn(move || {
            match start_stream(connection, overlay_cursor, embedded_region, target) {
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

        let node_id = node_id_rx.await??.await??;
        Ok(Self {
            node_id,
            thread_stop_tx,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    pub fn stop(&self) {
        let _ = self.thread_stop_tx.send(());
    }
}

#[derive(Debug)]
struct StreamingData {
    chosen_format: Option<Format>,
    chosen_modifier: Option<u64>,
    connection: WayshotConnection,
    overlay_cursor: bool,
    available_video_formats: Vec<VideoFormat>,
    embedded_region: Option<EmbeddedRegion>,
    size: libwayshot::Size,
    target: CastTarget,
    dmabuf_supported: bool,
    allow_shm_fallback: bool,
}

impl StreamingData {
    #[allow(clippy::too_many_arguments)]
    fn new(
        width: u32,
        height: u32,
        connection: WayshotConnection,
        overlay_cursor: bool,
        available_video_formats: Vec<VideoFormat>,
        embedded_region: Option<EmbeddedRegion>,
        target: CastTarget,
        dmabuf_supported: bool,
        allow_shm_fallback: bool,
    ) -> Self {
        Self {
            chosen_format: None,
            chosen_modifier: None,
            connection,
            overlay_cursor,
            available_video_formats,
            embedded_region,
            size: libwayshot::Size { width, height },
            target,
            dmabuf_supported,
            allow_shm_fallback,
        }
    }

    fn process(&mut self, stream: &pipewire::stream::Stream) {
        let buffer = unsafe { stream.dequeue_raw_buffer() };
        if buffer.is_null() {
            return;
        }

        let cast = unsafe {
            &mut *((*buffer).user_data as *mut libwayshot::screencast::WayshotScreenCast)
        };

        match self.connection.screencast(cast) {
            Err(libwayshot::Error::FramecopyFailedWithReason(WEnum::Value(
                FailureReason::BufferConstraints,
            ))) => {
                let size = cast.current_size();
                self.size = libwayshot::Size {
                    width: size
                        .width
                        .try_into()
                        .expect("capture width must be positive"),
                    height: size
                        .height
                        .try_into()
                        .expect("capture height must be positive"),
                };

                warn!(
                    "capture reported new buffer constraints: {}x{}",
                    self.size.width, self.size.height
                );

                let format = format(
                    self.size.width,
                    self.size.height,
                    &self.available_video_formats,
                );
                let buffers = buffers(
                    self.size.width,
                    self.size.height,
                    self.dmabuf_supported,
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
            Err(libwayshot::Error::FramecopyFailedWithReason(WEnum::Value(
                FailureReason::Stopped,
            ))) => {
                error!("capture target stopped");
                let _ = stream.set_active(false);
            }
            Err(err) => error!("frame copy failed: {err}"),
            Ok(_) => {}
        }

        unsafe { stream.queue_raw_buffer(buffer) };
    }

    fn add_buffer(&self, buffer: *mut pipewire::sys::pw_buffer) {
        let libwayshot::Size { width, height } = self.size;
        let buf = unsafe { &mut *(*buffer).buffer };
        let datas = unsafe { slice::from_raw_parts_mut(buf.datas, buf.n_datas as usize) };
        let wants_dmabuf =
            !datas.is_empty() && (datas[0].type_ & (1 << spa_sys::SPA_DATA_DmaBuf) != 0);

        if wants_dmabuf && self.dmabuf_supported {
            info!(
                "allocating DMA-BUF PipeWire buffer: size={}x{}, modifier={:?}",
                width, height, self.chosen_modifier
            );

            let unit = self
                .connection
                .create_screencast_with_dmabuf(
                    self.target.wayshot_target(),
                    self.overlay_cursor,
                    self.embedded_region,
                )
                .expect("DMA-BUF was probed before stream creation");
            let bo = unit
                .dmabuf_bo()
                .expect("DMA-BUF screencast must expose a BO");
            let plane_count = bo.plane_count() as usize;

            for (index, data) in datas.iter_mut().take(plane_count).enumerate() {
                let fd = bo.fd_for_plane(index as i32).expect("DMA-BUF plane fd");
                let offset = bo.offset(index as i32);
                let stride = bo.stride();

                debug!(
                    "DMA-BUF plane {index}: fd exported, offset={offset}, stride={stride}, maxsize={}",
                    width * height * 4
                );

                data.type_ = spa_sys::SPA_DATA_DmaBuf;
                data.flags = 0;
                data.fd = fd.into_raw_fd().into();
                data.data = std::ptr::null_mut();
                data.maxsize = width * height * 4;
                data.mapoffset = 0;

                let chunk = unsafe { &mut *data.chunk };
                chunk.size = height * stride;
                chunk.offset = offset;
                chunk.stride = stride as i32;
            }

            unsafe { (*buffer).user_data = Box::into_raw(Box::new(unit)) as *mut c_void };
            return;
        }

        if !self.allow_shm_fallback {
            panic!(
                "PipeWire did not allocate a DMA-BUF buffer although SHM fallback is disabled; datas[0].type={:?}, dmabuf_supported={}",
                datas.first().map(|data| data.type_),
                self.dmabuf_supported
            );
        }

        warn!(
            "allocating SHM PipeWire buffer because DMA-BUF was not selected; this is an explicit fallback"
        );
        assert_eq!(datas.len(), 1);
        let data = &mut datas[0];
        let name = c"xdg-desktop-portal-shiny";
        let fd = rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC)
            .expect("create memfd for SHM fallback");
        rustix::fs::ftruncate(&fd, (width * height * 4) as _).expect("resize memfd");

        let unit = self
            .connection
            .create_screencast_with_shm(
                self.target.wayshot_target(),
                self.overlay_cursor,
                self.chosen_format.unwrap_or(Format::Argb8888),
                self.embedded_region,
                &fd,
            )
            .expect("SHM screencast should be available when fallback is enabled");

        data.type_ = spa_sys::SPA_DATA_MemFd;
        data.flags = 0;
        data.fd = fd.into_raw_fd().into();
        data.data = std::ptr::null_mut();
        data.maxsize = width * height * 4;
        data.mapoffset = 0;

        let chunk = unsafe { &mut *data.chunk };
        chunk.size = width * height * 4;
        chunk.offset = 0;
        chunk.stride = 4 * width as i32;

        unsafe { (*buffer).user_data = Box::into_raw(Box::new(unit)) as *mut c_void };
    }

    fn remove_buffer(&self, buffer: *mut pipewire::sys::pw_buffer) {
        let buf = unsafe { &mut *(*buffer).buffer };
        let datas = unsafe { slice::from_raw_parts_mut(buf.datas, buf.n_datas as usize) };

        for data in datas {
            if data.fd >= 0 {
                unsafe { rustix::io::close(data.fd.try_into().expect("valid raw fd")) };
                data.fd = -1;
            }
        }

        if !unsafe { (*buffer).user_data }.is_null() {
            let cast: Box<libwayshot::screencast::WayshotScreenCast> =
                unsafe { Box::from_raw((*buffer).user_data as *mut _) };
            drop(cast);
            unsafe { (*buffer).user_data = std::ptr::null_mut() };
        }
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
                self.chosen_modifier = Some(info.modifier());
                self.chosen_format = spa_format_to_wl_shm(info.format());
                info!(
                    "PipeWire selected format={:?}, wl_shm={:?}, modifier=0x{:016x}",
                    info.format(),
                    self.chosen_format,
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
    oneshot::Receiver<anyhow::Result<u32>>,
);

fn start_stream(
    mut connection: WayshotConnection,
    overlay_cursor: bool,
    embedded_region: Option<EmbeddedRegion>,
    target: CastTarget,
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

    let wayshot_target = target.wayshot_target();
    let mut dmabuf_supported = connection.try_init_dmabuf(wayshot_target.clone()).is_ok();
    if dmabuf_supported {
        dmabuf_supported = connection
            .create_screencast_with_dmabuf(wayshot_target.clone(), overlay_cursor, embedded_region)
            .is_ok();
    }

    if dmabuf_supported {
        info!(
            "compositor capture path supports DMA-BUF; requesting DMA-BUF PipeWire buffers first"
        );
    } else if allow_shm_fallback {
        warn!("compositor capture path does not support DMA-BUF; falling back to SHM");
    } else {
        anyhow::bail!("compositor capture path does not support DMA-BUF");
    }

    let frame_format_list = connection.get_available_frame_formats(&wayshot_target)?;
    if frame_format_list.is_empty() {
        anyhow::bail!("capture target did not report any frame formats");
    }

    for frame_format in &frame_format_list {
        debug!(
            "available frame format: {:?}, size={}x{}",
            frame_format.format, frame_format.size.width, frame_format.size.height
        );
    }

    let libwayshot::Size { width, height } = frame_format_list[0].size;
    let available_frame_formats: Vec<Format> = frame_format_list
        .iter()
        .map(|frame_format| frame_format.format)
        .collect();
    let available_video_formats: Vec<VideoFormat> = preferred_formats(&available_frame_formats);
    if available_video_formats.is_empty() {
        anyhow::bail!("capture target formats could not be mapped to PipeWire SPA formats");
    }

    info!(
        "advertising PipeWire formats {:?} at {}x{} with linear modifier preferred",
        available_video_formats, width, height
    );

    let (node_id_tx, node_id_rx) = oneshot::channel();
    let mut node_id_tx = Some(node_id_tx);

    let listener = stream
        .add_local_listener_with_user_data(StreamingData::new(
            width,
            height,
            connection,
            overlay_cursor,
            available_video_formats.clone(),
            embedded_region,
            target,
            dmabuf_supported,
            allow_shm_fallback,
        ))
        .state_changed(move |stream, _, old, new| {
            info!("PipeWire stream state changed: {old:?} -> {new:?}");
            match new {
                StreamState::Paused => {
                    if let Some(tx) = node_id_tx.take() {
                        let _ = tx.send(Ok(stream.node_id()));
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

    let format = format(width, height, &available_video_formats);
    let buffers = buffers(width, height, dmabuf_supported, allow_shm_fallback);
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

fn preferred_formats(frame_formats: &[Format]) -> Vec<VideoFormat> {
    let preferred = [
        Format::Xrgb8888,
        Format::Argb8888,
        Format::Xbgr8888,
        Format::Abgr8888,
        Format::Xrgb2101010,
        Format::Argb2101010,
        Format::Xbgr2101010,
        Format::Abgr2101010,
    ];

    let mut result = Vec::new();
    for format in preferred {
        if frame_formats.contains(&format) {
            if let Some(spa_format) = wl_shm_format_to_spa(format) {
                result.push(spa_format);
            }
        }
    }

    for format in frame_formats {
        if let Some(spa_format) = wl_shm_format_to_spa(*format) {
            if !result.contains(&spa_format) {
                result.push(spa_format);
            }
        }
    }

    result
}

fn value_to_bytes(value: pod::Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut cursor = io::Cursor::new(&mut bytes);
    PodSerializer::serialize(&mut cursor, &value).expect("serialize pod value");
    bytes
}

fn buffers(width: u32, height: u32, dmabuf_supported: bool, allow_shm_fallback: bool) -> Vec<u8> {
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

fn format(width: u32, height: u32, available_video_formats: &[VideoFormat]) -> Vec<u8> {
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

    obj.properties.push(pod::Property {
        key: FormatProperties::VideoModifier.as_raw(),
        flags: pod::PropertyFlags::empty(),
        value: pod::Value::Choice(pod::ChoiceValue::Long(spa::utils::Choice(
            spa::utils::ChoiceFlags::empty(),
            spa::utils::ChoiceEnum::Enum {
                default: LINEAR_MODIFIER,
                alternatives: vec![LINEAR_MODIFIER],
            },
        ))),
    });

    value_to_bytes(pod::Value::Object(obj))
}

fn spa_format_to_wl_shm(format: VideoFormat) -> Option<Format> {
    match format {
        VideoFormat::BGRA => Some(Format::Argb8888),
        VideoFormat::BGRx => Some(Format::Xrgb8888),
        VideoFormat::ABGR => Some(Format::Rgba8888),
        VideoFormat::xBGR => Some(Format::Rgbx8888),
        VideoFormat::RGBA => Some(Format::Abgr8888),
        VideoFormat::RGBx => Some(Format::Xbgr8888),
        VideoFormat::ARGB => Some(Format::Bgra8888),
        VideoFormat::xRGB => Some(Format::Bgrx8888),
        VideoFormat::xRGB_210LE => Some(Format::Xrgb2101010),
        VideoFormat::xBGR_210LE => Some(Format::Xbgr2101010),
        VideoFormat::RGBx_102LE => Some(Format::Rgbx1010102),
        VideoFormat::BGRx_102LE => Some(Format::Bgrx1010102),
        VideoFormat::ARGB_210LE => Some(Format::Argb2101010),
        VideoFormat::ABGR_210LE => Some(Format::Abgr2101010),
        VideoFormat::RGBA_102LE => Some(Format::Rgba1010102),
        VideoFormat::BGRA_102LE => Some(Format::Bgra1010102),
        _ => None,
    }
}

fn wl_shm_format_to_spa(format: Format) -> Option<VideoFormat> {
    match format {
        Format::Argb8888 => Some(VideoFormat::BGRA),
        Format::Xrgb8888 => Some(VideoFormat::BGRx),
        Format::Rgba8888 => Some(VideoFormat::ABGR),
        Format::Rgbx8888 => Some(VideoFormat::xBGR),
        Format::Abgr8888 => Some(VideoFormat::RGBA),
        Format::Xbgr8888 => Some(VideoFormat::RGBx),
        Format::Bgra8888 => Some(VideoFormat::ARGB),
        Format::Bgrx8888 => Some(VideoFormat::xRGB),
        Format::Xrgb2101010 => Some(VideoFormat::xRGB_210LE),
        Format::Xbgr2101010 => Some(VideoFormat::xBGR_210LE),
        Format::Rgbx1010102 => Some(VideoFormat::RGBx_102LE),
        Format::Bgrx1010102 => Some(VideoFormat::BGRx_102LE),
        Format::Argb2101010 => Some(VideoFormat::ARGB_210LE),
        Format::Abgr2101010 => Some(VideoFormat::ABGR_210LE),
        Format::Rgba1010102 => Some(VideoFormat::RGBA_102LE),
        Format::Bgra1010102 => Some(VideoFormat::BGRA_102LE),
        _ => None,
    }
}
