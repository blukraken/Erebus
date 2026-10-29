//! Screenshot mode: one offscreen frame through the window's renderer, written as PNG.

use crate::gpu::Gpu;
use crate::render::{Renderer, View};
use std::path::PathBuf;

pub struct Shot {
    pub path: PathBuf,
    pub size: (u32, u32),
    pub view: View,
}

pub fn run(shot: &Shot) -> Result<(), String> {
    let (gpu, _) = Gpu::new(None)?;
    let (w, h) = shot.size;
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shot"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());

    let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shot readback"),
        size: row as u64 * h as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut renderer = Renderer::new(&gpu.device, &gpu.queue, format);
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    renderer.draw(&gpu, &mut encoder, &view, (w, h), 1.0, shot.view);
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| {
        if let Err(e) = r {
            log::error!("shot map: {e}");
        }
    });
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| format!("poll: {e}"))?;
    let data = slice.get_mapped_range().map_err(|e| format!("map: {e}"))?;

    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h as usize {
        let start = y * row as usize;
        for px in data[start..start + w as usize * 4].as_chunks::<4>().0 {
            rgb.extend_from_slice(&px[..3]);
        }
    }

    if let Some(dir) = shot.path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let file = std::fs::File::create(&shot.path).map_err(|e| format!("{}: {e}", shot.path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut writer = enc.write_header().map_err(|e| format!("png: {e}"))?;
    writer.write_image_data(&rgb).map_err(|e| format!("png: {e}"))?;
    log::info!("screenshot {}x{} {:?} -> {}", w, h, shot.view, shot.path.display());
    Ok(())
}
