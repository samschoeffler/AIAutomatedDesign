//! Drawing the part with the GPU (wgpu), inside egui's own frame.
//!
//! egui lets a widget hand it a *paint callback*: code that runs while egui is drawing the
//! window, with access to the GPU. The viewer builds a [`Scene`] each frame (camera, which
//! meshes, which colours) and egui calls it back:
//! 1. [`CallbackTrait::prepare`]: copy any new meshes and this frame's settings to the GPU;
//! 2. [`CallbackTrait::paint`]: issue the draw calls.
//!
//! The GPU objects that live across frames (pipelines, buffers) are kept in [`Gpu`], stored
//! in egui's `callback_resources`.

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu, CallbackTrait};
use std::collections::HashMap;
use std::sync::Arc;
use wgpu::util::DeviceExt;

/// Depth buffer format; main.rs asks eframe for a 32-bit depth buffer to match.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Multisample anti-aliasing (smooth edges); main.rs asks eframe for the same.
pub const MSAA_SAMPLES: u32 = 4;

/// A triangle corner as the GPU sees it.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SolidVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
}

/// One end of a line segment.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LineVertex {
    pub position: [f32; 3],
    pub color: [f32; 4],
}

/// Triangles and lines ready to go to the GPU. `id` is unique per mesh, so the GPU copy
/// is only made once.
#[derive(Debug, Default)]
pub struct MeshData {
    pub id: u64,
    pub triangles: Vec<SolidVertex>,
    /// Pairs of points: each two make one segment.
    pub lines: Vec<LineVertex>,
}

/// Which mesh a draw uses. Each slot holds one mesh on the GPU at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Design,
    Proposal,
    Grid,
}

/// How to draw a mesh.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    /// Opaque triangles.
    Solid,
    /// See-through triangles (they don't hide what's behind them).
    Ghost,
    /// The mesh's lines.
    Lines,
}

#[derive(Clone, Copy, Debug)]
pub struct Draw {
    pub slot: Slot,
    pub style: Style,
    /// RGBA, 0..1. For lines it tints the per-vertex colours.
    pub color: [f32; 4],
}

/// Everything needed to draw one frame of the viewer.
pub struct Scene {
    pub view_proj: glam::Mat4,
    /// Direction towards the light.
    pub light: glam::Vec3,
    /// The current mesh for each slot that's drawn.
    pub meshes: Vec<(Slot, Arc<MeshData>)>,
    /// Drawn in this order.
    pub draws: Vec<Draw>,
}

/// The per-draw settings, laid out exactly like `Uniforms` in shader.wgsl.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
    color: [f32; 4],
    light: [f32; 4],
}

struct GpuMesh {
    id: u64,
    triangles: Option<(wgpu::Buffer, u32)>,
    lines: Option<(wgpu::Buffer, u32)>,
}

/// One uniform buffer and its bind group; one per draw in a frame.
struct UniformSlot {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

/// The viewer's long-lived GPU objects.
pub struct Gpu {
    solid: wgpu::RenderPipeline,
    ghost: wgpu::RenderPipeline,
    lines: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniforms: Vec<UniformSlot>,
    meshes: HashMap<Slot, GpuMesh>,
}

/// Set up the GPU side once, when the window opens.
pub fn init(render_state: &egui_wgpu::RenderState) {
    let device = &render_state.device;
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("viewer shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("viewer uniforms"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("viewer"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });

    let solid_buffers = [Some(wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<SolidVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
    })];
    let line_buffers = [Some(wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<LineVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
    })];

    // One function makes all three pipelines; they differ in a few settings.
    let make = |label: &str,
                vs: &str,
                fs: &str,
                buffers: &[Option<wgpu::VertexBufferLayout>],
                topology: wgpu::PrimitiveTopology,
                blend: bool,
                depth_write: bool| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some(vs),
                compilation_options: Default::default(),
                buffers,
            },
            primitive: wgpu::PrimitiveState {
                topology,
                // Draw both sides: holes and cuts show their inside faces.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(depth_write),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState { count: MSAA_SAMPLES, ..Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(fs),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: render_state.target_format,
                    blend: blend.then_some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        })
    };
    let tri = wgpu::PrimitiveTopology::TriangleList;
    let gpu = Gpu {
        solid: make("solid", "vs_solid", "fs_solid", &solid_buffers, tri, false, true),
        ghost: make("ghost", "vs_solid", "fs_solid", &solid_buffers, tri, true, false),
        lines: make("lines", "vs_line", "fs_line", &line_buffers, wgpu::PrimitiveTopology::LineList, true, false),
        layout,
        uniforms: Vec::new(),
        meshes: HashMap::new(),
    };
    render_state.renderer.write().callback_resources.insert(gpu);
}

fn upload(device: &wgpu::Device, mesh: &MeshData) -> GpuMesh {
    let buffer = |label: &str, bytes: &[u8]| {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: bytes,
            usage: wgpu::BufferUsages::VERTEX,
        })
    };
    GpuMesh {
        id: mesh.id,
        triangles: (!mesh.triangles.is_empty()).then(|| {
            (buffer("triangles", bytemuck::cast_slice(&mesh.triangles)), mesh.triangles.len() as u32)
        }),
        lines: (!mesh.lines.is_empty())
            .then(|| (buffer("lines", bytemuck::cast_slice(&mesh.lines)), mesh.lines.len() as u32)),
    }
}

impl CallbackTrait for Scene {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(gpu) = resources.get_mut::<Gpu>() else { return Vec::new() };

        // New meshes go to the GPU; unchanged ones stay.
        for (slot, mesh) in &self.meshes {
            if gpu.meshes.get(slot).is_none_or(|m| m.id != mesh.id) {
                gpu.meshes.insert(*slot, upload(device, mesh));
            }
        }

        // One uniform buffer per draw (made once, then reused).
        while gpu.uniforms.len() < self.draws.len() {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("viewer uniforms"),
                size: std::mem::size_of::<Uniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("viewer uniforms"),
                layout: &gpu.layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
            });
            gpu.uniforms.push(UniformSlot { buffer, bind_group });
        }
        let light = self.light.normalize_or_zero();
        for (draw, slot) in self.draws.iter().zip(&gpu.uniforms) {
            let u = Uniforms {
                view_proj: self.view_proj.to_cols_array_2d(),
                color: draw.color,
                light: [light.x, light.y, light.z, 0.0],
            };
            queue.write_buffer(&slot.buffer, 0, bytemuck::bytes_of(&u));
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(gpu) = resources.get::<Gpu>() else { return };
        for (draw, uniforms) in self.draws.iter().zip(&gpu.uniforms) {
            let Some(mesh) = gpu.meshes.get(&draw.slot) else { continue };
            let (pipeline, buffer) = match draw.style {
                Style::Solid => (&gpu.solid, &mesh.triangles),
                Style::Ghost => (&gpu.ghost, &mesh.triangles),
                Style::Lines => (&gpu.lines, &mesh.lines),
            };
            let Some((buffer, count)) = buffer else { continue };
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &uniforms.bind_group, &[]);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..*count, 0..1);
        }
    }
}
