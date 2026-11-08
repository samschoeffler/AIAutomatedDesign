//! The `render` tool (docs/SPEC.md, phase 5): a PNG picture of the part, so Claude can look
//! at what it made.
//!
//! It draws exactly what the window's 3D view draws (same [`Gpu`] pipelines, same scene,
//! same colours), but into an image in memory instead of the window. It has its own
//! connection to the GPU, set up the first time it's needed and kept after that, so it
//! works whether or not the window is showing and never gets in the window's way.

use super::build::{build, Built};
use super::gpu::{Gpu, DEPTH_FORMAT, MSAA_SAMPLES};
use super::viewer::{grid_key, make_grid, scene, Camera, Compare, Content, View};
use aad_model::Design;
use base64::Engine;
use eframe::wgpu;
use std::sync::{Mutex, OnceLock};

/// The image format: 8-bit RGBA, not sRGB, the same as the window draws into.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// The 3D view's background (the window's viewer panel colour).
const BACKGROUND: [u8; 3] = [38, 41, 48];
/// Lines between the four pictures of an "all views" image.
const DIVIDER: [u8; 3] = [90, 95, 105];

pub const MIN_SIZE: u32 = 200;
pub const MAX_SIZE: u32 = 2000;

/// What to draw.
pub struct Request<'a> {
    /// The accepted design.
    pub design: &'a Design,
    /// Claude's proposal, to show instead (blue, with the current part's edges in orange).
    pub proposal: Option<&'a Design>,
    /// One view, or `None` for all four in a 2 x 2 grid.
    pub view: Option<View>,
    /// Width and height of the whole picture, in pixels.
    pub size: u32,
}

/// Draw the picture and return it as PNG bytes.
pub fn render_png(req: &Request) -> Result<Vec<u8>, String> {
    let design = build(req.design)?;
    let proposal = req.proposal.map(build).transpose().map_err(|e| format!("the proposal doesn't build: {e}"))?;
    let size = req.size.clamp(MIN_SIZE, MAX_SIZE);

    let mut renderer = renderer().lock().unwrap_or_else(|e| e.into_inner());
    let renderer = renderer.as_mut().map_err(|e| e.clone())?;
    let pixels = renderer.draw(&design, proposal.as_ref(), req.view, size)?;
    encode_png(&pixels, size, size)
}

/// PNG bytes as base64 text, the way MCP sends images.
pub fn base64(png: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(png)
}

/// The views in an "all" picture, left to right, top to bottom.
pub const ALL_VIEWS: [View; 4] = [View::Iso, View::Top, View::Front, View::Right];

/// The GPU connection, made once. If there's no usable GPU the error is kept, so every
/// request gets the same clear answer instead of trying again.
fn renderer() -> &'static Mutex<Result<Renderer, String>> {
    static RENDERER: OnceLock<Mutex<Result<Renderer, String>>> = OnceLock::new();
    RENDERER.get_or_init(|| Mutex::new(Renderer::new()))
}

struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    gpu: Gpu,
}

impl Renderer {
    fn new() -> Result<Renderer, String> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .map_err(|e| format!("no GPU to render with: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| format!("couldn't start the GPU for rendering: {e}"))?;
        let gpu = Gpu::new(&device, FORMAT);
        Ok(Renderer { device, queue, gpu })
    }

    /// Draw into a `size` x `size` image; returns its RGBA pixels, row by row.
    fn draw(&mut self, design: &Built, proposal: Option<&Built>, view: Option<View>, size: u32) -> Result<Vec<u8>, String> {
        let texture = |label, samples, usage| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format: if label == "depth" { DEPTH_FORMAT } else { FORMAT },
                usage,
                view_formats: &[],
            })
        };
        // Drawn with multisampling (smooth edges), then resolved into a plain image to read back.
        let msaa = texture("msaa", MSAA_SAMPLES, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let depth = texture("depth", MSAA_SAMPLES, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let image = texture("image", 1, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let (msaa, depth, image_view) = (
            msaa.create_view(&Default::default()),
            depth.create_view(&Default::default()),
            image.create_view(&Default::default()),
        );

        let bounds = [Some(design), proposal]
            .into_iter()
            .flatten()
            .filter_map(|b| b.bounds)
            .reduce(|(lo1, hi1), (lo2, hi2)| (lo1.min(lo2), hi1.max(hi2)));
        let content = Content {
            design: Some(design.mesh.clone()),
            proposal: proposal.map(|p| p.mesh.clone()),
            compare: Compare::Proposed,
            bounds,
        };
        let grid = make_grid(grid_key(bounds));

        // One tile for a single view, four for all of them.
        let tiles: Vec<(View, [f32; 4])> = match view {
            Some(v) => vec![(v, [0.0, 0.0, size as f32, size as f32])],
            None => {
                let half = size as f32 / 2.0;
                ALL_VIEWS
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (*v, [(i % 2) as f32 * half, (i / 2) as f32 * half, half, half]))
                    .collect()
            }
        };

        // Each tile is its own submission: the per-draw settings are rewritten for each.
        for (i, (v, [x, y, w, h])) in tiles.iter().enumerate() {
            let mut camera = Camera::new();
            camera.look_from(*v);
            if let Some((lo, hi)) = bounds {
                camera.fit(lo, hi, w / h);
            }
            let scene = scene(&camera, w / h, &content, grid.clone());
            self.gpu.prepare(&self.device, &self.queue, &scene);

            let mut encoder = self.device.create_command_encoder(&Default::default());
            {
                let bg = BACKGROUND.map(|c| c as f64 / 255.0);
                let first = i == 0;
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("render tool"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &msaa,
                        resolve_target: Some(&image_view),
                        ops: wgpu::Operations {
                            load: if first {
                                wgpu::LoadOp::Clear(wgpu::Color { r: bg[0], g: bg[1], b: bg[2], a: 1.0 })
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &depth,
                        depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                        stencil_ops: None,
                    }),
                    ..Default::default()
                });
                pass.set_viewport(*x, *y, *w, *h, 0.0, 1.0);
                pass.set_scissor_rect(*x as u32, *y as u32, *w as u32, *h as u32);
                self.gpu.paint(&mut pass, &scene);
            }
            self.queue.submit([encoder.finish()]);
        }

        let mut pixels = self.read_back(&image, size)?;
        if view.is_none() {
            draw_dividers(&mut pixels, size);
        }
        Ok(pixels)
    }

    /// Copy the finished image from the GPU into memory.
    fn read_back(&self, image: &wgpu::Texture, size: u32) -> Result<Vec<u8>, String> {
        // The GPU copies whole rows padded to a multiple of 256 bytes.
        let row = size * 4;
        let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("render read-back"),
            size: (padded * size) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: image,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(size) },
            },
            wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| format!("the GPU didn't finish: {e}"))?;
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| format!("couldn't read the picture back: {e}"))?;

        let data = slice.get_mapped_range().map_err(|e| format!("couldn't read the picture back: {e}"))?;
        let mut pixels = Vec::with_capacity((row * size) as usize);
        for r in 0..size {
            let start = (r * padded) as usize;
            pixels.extend_from_slice(&data[start..start + row as usize]);
        }
        drop(data);
        buffer.unmap();
        Ok(pixels)
    }
}

/// A cross between the four tiles of an "all views" picture.
fn draw_dividers(pixels: &mut [u8], size: u32) {
    let half = size / 2;
    let mut put = |x: u32, y: u32| {
        let i = ((y * size + x) * 4) as usize;
        pixels[i..i + 3].copy_from_slice(&DIVIDER);
        pixels[i + 3] = 255;
    };
    for t in 0..size {
        for d in 0..2 {
            put(half - 1 + d, t);
            put(t, half - 1 + d);
        }
    }
}

fn encode_png(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(pixels).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Shared with control.rs so the tool's text can say what's where.
pub fn describe(view: Option<View>, with_proposal: bool) -> String {
    let layout = match view {
        Some(v) => format!("{} view", v.label()),
        None => "four views: Iso (top left), Top (top right), Front (bottom left), Right (bottom right)".into(),
    };
    let colours = if with_proposal {
        "The proposed part is blue; orange lines are the current part's edges where they differ."
    } else {
        "The part is grey with dark edges."
    };
    format!(
        "{layout}. {colours} The grid is on the XY plane in 10 mm squares (50 mm for big parts; stronger lines every 50 mm); \
         the red line is the X axis, green Y, blue Z (up). Top looks down Z with X to the right; \
         Front looks along +Y with X to the right; Right looks along -X with Y to the right."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plate() -> Design {
        Design::from_json(
            r#"{ "name": "Plate", "features": [
            { "id": "s1", "type": "sketch", "plane": "XY", "entities": [ { "type": "rect", "x0": 0, "y0": 0, "x1": 40, "y1": 20 } ] },
            { "id": "e1", "type": "extrude", "sketch": "s1", "distance": 3 } ] }"#,
        )
        .expect("loads")
    }

    fn pixel(png_bytes: &[u8], x: u32, y: u32) -> [u8; 4] {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().expect("png header");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("png data");
        let i = ((y * info.width + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    /// Skips (passes) on a machine without a GPU, such as some CI runners.
    fn gpu_available() -> bool {
        match renderer().lock().map(|r| r.is_ok()) {
            Ok(true) => true,
            _ => {
                eprintln!("no GPU here; skipping the render test");
                false
            }
        }
    }

    #[test]
    fn renders_the_part_in_the_middle_on_the_background() {
        if !gpu_available() {
            return;
        }
        let design = plate();
        let png_bytes = render_png(&Request { design: &design, proposal: None, view: Some(View::Top), size: 300 })
            .expect("renders");
        assert_eq!(&png_bytes[1..4], b"PNG");
        let corner = pixel(&png_bytes, 2, 2);
        assert_eq!(&corner[..3], &BACKGROUND, "the corner is background");
        // From the top, the plate fills the middle: light grey, much brighter than the background.
        let middle = pixel(&png_bytes, 150, 150);
        assert!(middle[0] > 120 && middle[1] > 120 && middle[2] > 120, "the middle is the part: {middle:?}");
    }

    #[test]
    fn a_proposal_is_blue_and_all_views_has_four_tiles() {
        if !gpu_available() {
            return;
        }
        let design = plate();
        let mut thicker = plate();
        if let aad_model::Feature::Extrude { distance, .. } = &mut thicker.features[1] {
            *distance = 6.0;
        }
        let png_bytes = render_png(&Request { design: &design, proposal: Some(&thicker), view: None, size: 400 })
            .expect("renders");
        // The divider cross is in the middle.
        assert_eq!(&pixel(&png_bytes, 200, 50)[..3], &DIVIDER);
        // The Top tile (top right) shows the proposal from above: blue.
        let top = pixel(&png_bytes, 300, 100);
        assert!(top[2] > top[0] + 40, "the proposal is blue: {top:?}");
    }
}

