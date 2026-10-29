//! Instance, adapter and device. D3D12 first, then any primary backend.

use std::sync::Arc;
use winit::window::Window;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter: wgpu::Adapter,
    pub instance: wgpu::Instance,
}

impl Gpu {
    /// With a window, also returns a surface made from the chosen instance.
    pub fn new(window: Option<Arc<Window>>) -> Result<(Gpu, Option<wgpu::Surface<'static>>), String> {
        for backends in [wgpu::Backends::DX12, wgpu::Backends::PRIMARY] {
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
            desc.backends = backends;
            let instance = wgpu::Instance::new(desc);

            let surface = match &window {
                Some(w) => match instance.create_surface(w.clone()) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        log::warn!("surface on {backends:?}: {e}");
                        continue;
                    }
                },
                None => None,
            };

            let opts = wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: surface.as_ref(),
                apply_limit_buckets: false,
            };
            let adapter = match pollster::block_on(instance.request_adapter(&opts)) {
                Ok(a) => a,
                Err(e) => {
                    log::warn!("no adapter on {backends:?}: {e}");
                    continue;
                }
            };

            // Offscreen runs time their passes when the adapter allows it.
            let timestamps = if window.is_none() {
                adapter.features() & wgpu::Features::TIMESTAMP_QUERY
            } else {
                wgpu::Features::empty()
            };
            let desc = wgpu::DeviceDescriptor {
                label: Some("erebus-face"),
                required_features: timestamps,
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            };
            let (device, queue) = match pollster::block_on(adapter.request_device(&desc)) {
                Ok(dq) => dq,
                Err(e) => {
                    log::warn!("device on {backends:?}: {e}");
                    continue;
                }
            };

            let info = adapter.get_info();
            log::info!(
                "adapter: {} ({:?}), driver {} {}",
                info.name,
                info.backend,
                info.driver,
                info.driver_info
            );
            return Ok((Gpu { device, queue, adapter, instance }, surface));
        }
        Err("no usable GPU adapter".into())
    }
}
