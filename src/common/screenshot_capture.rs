use std::{
    fs::{File, OpenOptions},
    io::Read,
    os::{fd::AsFd, unix::ffi::OsStrExt, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use wayland_client::protocol::wl_shm;

use crate::common::{
    shell_ipc::CustomRegion,
    wayland_capture::{CaptureTarget, DamageSet, DirectCapture, ShmFormat},
};

static SCREENSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pixels: Vec<u8>,
}

impl RgbaImage {
    fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        let len = image_byte_len(width, height)?;
        Ok(Self {
            width,
            height,
            pixels: vec![0; len],
        })
    }

    pub fn crop(&self, x: i64, y: i64, width: i64, height: i64) -> anyhow::Result<Self> {
        let x1 = x.clamp(0, self.width as i64);
        let y1 = y.clamp(0, self.height as i64);
        let x2 = (x + width).clamp(0, self.width as i64);
        let y2 = (y + height).clamp(0, self.height as i64);
        if x2 <= x1 || y2 <= y1 {
            anyhow::bail!("selected screenshot region is empty");
        }

        let mut result = Self::new((x2 - x1) as u32, (y2 - y1) as u32)?;
        for row in 0..result.height as usize {
            let source_start = ((y1 as usize + row) * self.width as usize + x1 as usize) * 4;
            let source_end = source_start + result.width as usize * 4;
            let target_start = row * result.width as usize * 4;
            result.pixels[target_start..target_start + result.width as usize * 4]
                .copy_from_slice(&self.pixels[source_start..source_end]);
        }

        Ok(result)
    }
}

pub fn capture_target(
    capture: &mut DirectCapture,
    target: &CaptureTarget,
    paint_cursors: bool,
) -> anyhow::Result<RgbaImage> {
    let probe = capture.probe(target, paint_cursors)?;
    let format = choose_readable_shm_format(&probe.shm_formats).ok_or_else(|| {
        anyhow::anyhow!("compositor advertised no readable SHM screenshot format")
    })?;

    let fd = rustix::fs::memfd_create(
        c"xdg-desktop-portal-shiny-screenshot",
        rustix::fs::MemfdFlags::CLOEXEC,
    )?;

    rustix::fs::ftruncate(&fd, format.byte_size())?;

    let buffer = capture.create_shm_buffer(target, Some(format.format), fd.as_fd())?;
    let actual_format = buffer
        .shm_format()
        .ok_or_else(|| anyhow::anyhow!("screenshot capture created a non-SHM buffer"))?;

    if actual_format.byte_size() > format.byte_size() {
        anyhow::bail!("screenshot buffer constraints changed during allocation");
    }

    capture
        .capture_into_buffer(target, paint_cursors, &buffer, &DamageSet::full())
        .map_err(anyhow::Error::from)?;

    let mut bytes = vec![0; actual_format.byte_size().try_into()?];
    let mut file: File = fd.into();
    file.read_exact(&mut bytes)?;
    shm_to_rgba(&bytes, actual_format)
}

pub fn capture_region(
    capture: &mut DirectCapture,
    region: &CustomRegion,
    paint_cursors: bool,
) -> anyhow::Result<RgbaImage> {
    let output = capture
        .outputs()
        .iter()
        .find(|output| output.name == region.monitor)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("selected monitor {} was not found", region.monitor))?;

    if output.size.width == 0 || output.size.height == 0 {
        anyhow::bail!("selected monitor has invalid logical dimensions");
    }

    let image = capture_target(
        capture,
        &CaptureTarget::Output(output.output),
        paint_cursors,
    )?;

    let x = region.x as i64 * image.width as i64 / output.size.width as i64;
    let y = region.y as i64 * image.height as i64 / output.size.height as i64;
    let width = region.width as i64 * image.width as i64 / output.size.width as i64;
    let height = region.height as i64 * image.height as i64 / output.size.height as i64;
    image.crop(x, y, width, height)
}

pub fn write_png(image: &RgbaImage) -> anyhow::Result<String> {
    let directory = screenshot_directory();
    std::fs::create_dir_all(&directory)?;

    let sequence = SCREENSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!(
        "shiny-screenshot-{}-{sequence}.png",
        std::process::id()
    ));

    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;

    let mut encoder = png::Encoder::new(file, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&image.pixels)?;
    writer.finish()?;

    Ok(path_to_file_uri(&path))
}

fn choose_readable_shm_format(formats: &[ShmFormat]) -> Option<ShmFormat> {
    const PREFERRED: &[wl_shm::Format] = &[
        wl_shm::Format::Argb8888,
        wl_shm::Format::Xrgb8888,
        wl_shm::Format::Abgr8888,
        wl_shm::Format::Xbgr8888,
        wl_shm::Format::Argb2101010,
        wl_shm::Format::Xrgb2101010,
        wl_shm::Format::Abgr2101010,
        wl_shm::Format::Xbgr2101010,
    ];

    PREFERRED.iter().find_map(|candidate| {
        formats
            .iter()
            .find(|format| format.format == *candidate)
            .copied()
    })
}

fn shm_to_rgba(bytes: &[u8], format: ShmFormat) -> anyhow::Result<RgbaImage> {
    let mut image = RgbaImage::new(format.size.width, format.size.height)?;
    let stride = format.stride as usize;

    for y in 0..format.size.height as usize {
        for x in 0..format.size.width as usize {
            let source_offset = y * stride + x * 4;
            let target_offset = (y * format.size.width as usize + x) * 4;
            let pixel = u32::from_ne_bytes(
                bytes[source_offset..source_offset + 4]
                    .try_into()
                    .expect("four-byte SHM pixel"),
            );
            let rgba = unpack_shm_pixel(pixel, format.format)?;
            image.pixels[target_offset..target_offset + 4].copy_from_slice(&rgba);
        }
    }

    Ok(image)
}

fn unpack_shm_pixel(pixel: u32, format: wl_shm::Format) -> anyhow::Result<[u8; 4]> {
    let result = match format {
        wl_shm::Format::Argb8888 => [
            (pixel >> 16) as u8,
            (pixel >> 8) as u8,
            pixel as u8,
            (pixel >> 24) as u8,
        ],
        wl_shm::Format::Xrgb8888 => [(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8, 255],
        wl_shm::Format::Abgr8888 => [
            pixel as u8,
            (pixel >> 8) as u8,
            (pixel >> 16) as u8,
            (pixel >> 24) as u8,
        ],
        wl_shm::Format::Xbgr8888 => [pixel as u8, (pixel >> 8) as u8, (pixel >> 16) as u8, 255],
        wl_shm::Format::Argb2101010 => [
            ten_to_eight((pixel >> 20) & 0x3ff),
            ten_to_eight((pixel >> 10) & 0x3ff),
            ten_to_eight(pixel & 0x3ff),
            two_to_eight(pixel >> 30),
        ],
        wl_shm::Format::Xrgb2101010 => [
            ten_to_eight((pixel >> 20) & 0x3ff),
            ten_to_eight((pixel >> 10) & 0x3ff),
            ten_to_eight(pixel & 0x3ff),
            255,
        ],
        wl_shm::Format::Abgr2101010 => [
            ten_to_eight(pixel & 0x3ff),
            ten_to_eight((pixel >> 10) & 0x3ff),
            ten_to_eight((pixel >> 20) & 0x3ff),
            two_to_eight(pixel >> 30),
        ],
        wl_shm::Format::Xbgr2101010 => [
            ten_to_eight(pixel & 0x3ff),
            ten_to_eight((pixel >> 10) & 0x3ff),
            ten_to_eight((pixel >> 20) & 0x3ff),
            255,
        ],
        _ => anyhow::bail!("unsupported screenshot SHM format {format:?}"),
    };
    Ok(result)
}

fn ten_to_eight(value: u32) -> u8 {
    ((value * 255 + 511) / 1023) as u8
}

fn two_to_eight(value: u32) -> u8 {
    ((value & 0x3) * 85) as u8
}

fn image_byte_len(width: u32, height: u32) -> anyhow::Result<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("screenshot dimensions overflow address space"))
}

fn screenshot_directory() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("xdg-desktop-portal-shiny")
}

fn path_to_file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(*byte as char)
            }
            _ => {
                use std::fmt::Write;
                write!(uri, "%{byte:02X}").expect("writing to String cannot fail");
            }
        }
    }
    uri
}
