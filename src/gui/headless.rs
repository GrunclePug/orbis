//! Headless mode: run the interface without a window, render the last
//! frame offscreen with wgpu (lavapipe works), and write a PNG and a text
//! dump of the state. This is how GUI changes get checked.

use std::fs::File;
use std::future::Future;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, Wake, Waker};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use eframe::egui;
use eframe::egui_wgpu;
use eframe::wgpu;

use super::{App, Options};
use crate::cli::Context;

pub fn run(ctx: Context, opts: &Options, size: [f32; 2]) -> Result<()> {
    let egui_ctx = egui::Context::default();
    let mut app = App::new(ctx, opts, egui_ctx.clone());
    app.load_index_now();

    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(size[0], size[1]));
    let keys: Vec<char> = opts.keys.as_deref().unwrap_or("").chars().collect();
    let frames = opts.frames.max(1).max(
        u32::try_from(keys.len())
            .unwrap_or(u32::MAX)
            .saturating_add(2),
    );
    let mut outputs = Vec::new();
    for frame in 0..frames {
        // Typed keys start on the second frame, after the first layout.
        let events = usize::try_from(frame)
            .ok()
            .and_then(|f| f.checked_sub(1))
            .and_then(|i| keys.get(i))
            .map(|c| vec![egui::Event::Text(c.to_string())])
            .unwrap_or_default();
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            time: Some(f64::from(frame) / 60.0),
            events,
            ..Default::default()
        };
        let out = egui_ctx.run_ui(raw, |ui| app.frame(ui));
        outputs.push(out);
    }

    if let Some(path) = &opts.dump {
        std::fs::write(path, app.dump()).with_context(|| format!("write {}", path.display()))?;
    }
    match &opts.screenshot {
        Some(path) => render(&egui_ctx, outputs, size, path)?,
        None => {
            // Nothing to draw: drop the texture updates deliberately.
            for mut out in outputs {
                out.textures_delta.clear();
            }
        }
    }
    Ok(())
}

/// Polls a future to completion on this thread.
fn block_on<F: Future>(fut: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = TaskContext::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park_timeout(Duration::from_millis(10)),
        }
    }
}

fn render(
    ctx: &egui::Context,
    outputs: Vec<egui::FullOutput>,
    size: [f32; 2],
    path: &Path,
) -> Result<()> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .map_err(|e| anyhow!("no GPU adapter (install a Vulkan driver such as lavapipe): {e}"))?;
    log::info!("headless adapter: {:?}", adapter.get_info().name);
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .map_err(|e| anyhow!("request GPU device: {e}"))?;

    let ppp = ctx.pixels_per_point();
    // Size in points times the scale gives whole pixels; truncation is fine.
    let width = (size[0] * ppp).round().max(1.0) as u32;
    let height = (size[1] * ppp).round().max(1.0) as u32;
    // egui paints in gamma space, which is what a PNG stores.
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("orbis-headless"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer =
        egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());

    // Apply every frame's texture changes (the font atlas arrives early),
    // then draw the last frame.
    let mut last = None;
    for mut out in outputs {
        for (id, deltas) in &out.textures_delta.set {
            for delta in deltas {
                renderer.update_texture(&device, &queue, *id, delta);
            }
        }
        for id in &out.textures_delta.free {
            renderer.free_texture(id);
        }
        out.textures_delta.clear();
        last = Some(out);
    }
    let last = last.context("no frames rendered")?;
    let jobs = ctx.tessellate(last.shapes, last.pixels_per_point);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point: last.pixels_per_point,
    };

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    let extra = renderer.update_buffers(&device, &queue, &mut encoder, &jobs, &screen);
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("egui"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.02,
                        g: 0.02,
                        b: 0.02,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        let mut pass = pass.forget_lifetime();
        renderer.render(&mut pass, &jobs, &screen);
    }

    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let row = width * 4;
    let padded = row.div_ceil(align) * align;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(extra.into_iter().chain(std::iter::once(encoder.finish())));

    buffer.map_async(wgpu::MapMode::Read, .., |r| {
        if let Err(e) = r {
            log::error!("map readback buffer: {e}");
        }
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| anyhow!("wait for GPU: {e}"))?;
    let data = buffer
        .get_mapped_range(..)
        .map_err(|e| anyhow!("read back the frame: {e}"))?;
    let mut pixels = Vec::with_capacity((row * height) as usize);
    for y in 0..height {
        let start = (y * padded) as usize;
        pixels.extend_from_slice(&data[start..start + row as usize]);
    }
    drop(data);
    buffer.unmap();

    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut enc = png::Encoder::new(BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().context("write PNG header")?;
    writer.write_image_data(&pixels).context("write PNG data")?;
    writer.finish().context("finish PNG")?;
    Ok(())
}
