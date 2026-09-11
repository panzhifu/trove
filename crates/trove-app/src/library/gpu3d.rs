//! GPU renderer for the 3D model viewport.
//!
//! The viewport needs a device of its own: gpui keeps its wgpu device private
//! (`Window::gpu_specs` only describes it), so this module builds a second one
//! and renders off-screen, then reads the pixels back for gpui to draw as an
//! image. The read-back costs a copy, but the rasterization — the part that
//! grows with the triangle count — runs on the GPU, which is the point: a mesh
//! with a million triangles turns at interactive speed instead of taking tens
//! of milliseconds per frame on the CPU.
//!
//! Everything expensive is reused. The device, pipelines and uniform buffer are
//! built once per viewport; the vertex and index buffers are built once per
//! model. Only the uniform block changes as the camera moves, and the
//! off-screen targets follow the viewport size.
//!
//! wgpu's handles are `Send + Sync` on native targets, so a frame can be
//! rendered from a background task without blocking the UI thread.

use std::sync::mpsc;

use trove_core::media::gpu::{self, UNIFORM_SIZE, Uniforms};
use trove_core::media::mesh::Mesh;
use trove_core::media::render3d::{self, Framing};

/// Off-screen colour format: plain 8-bit RGBA, so the bytes read back are the
/// perceptual values the shader wrote — the same ones the CPU path emits.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Depth buffer for the model pass.
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

const SHADER: &str = include_str!("gpu3d.wgsl");

/// Why the GPU path could not be started.
///
/// The variants are the reasons worth telling the user apart, not the raw
/// driver message: the viewport shows a localized sentence built from one of
/// them, with the technical detail appended where there is any.
#[derive(Debug, Clone)]
pub enum GpuUnavailable {
    /// No adapter matched at all — what a VM, a remote session, or a machine
    /// whose driver the process cannot open looks like.
    NoAdapter,
    /// Only a software rasterizer came up. It would be slower than the CPU
    /// path this falls back to, so it is refused on purpose. Carries its name.
    Software(String),
    /// An adapter was found but no device could be created from it. Carries
    /// the backend's own message.
    NoDevice(String),
}

/// A model whose geometry already lives in GPU buffers.
///
/// A triangle mesh puts its interleaved vertices in `vertices` and indexes
/// them; a point cloud puts one instance per point there instead and is drawn
/// with the point pipeline.
pub struct GpuMesh {
    vertices: wgpu::Buffer,
    indices: Option<wgpu::Buffer>,
    vertex_count: u32,
    index_count: u32,
    /// Points to draw; `0` when this is a triangle mesh.
    point_count: u32,
}

/// Device, pipelines and the resources shared by every frame.
pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    model_pipeline: wgpu::RenderPipeline,
    point_pipeline: wgpu::RenderPipeline,
    backdrop_pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// MSAA sample count, resolved from the adapter's capabilities. 1 when the
    /// format cannot be multisampled; a model's silhouette is where aliasing
    /// shows worst, so this is worth asking for.
    samples: u32,
    /// Adapter description, shown in the status line.
    pub adapter: String,
}

impl GpuRenderer {
    /// Bring up a device, or explain why not. Blocking: callers run this off
    /// the UI thread.
    pub fn new() -> Result<Self, GpuUnavailable> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            // Vulkan first — that is what the desktop drivers expose — with
            // GLES as the fallback for older or software setups.
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });

        // On Optimus / hybrid-graphics laptops `request_adapter` with a
        // `HighPerformance` hint can still hand us the integrated GPU, so
        // enumerate every adapter and pick the discrete one. The fallback
        // chain: discrete → non-software → whatever is there.
        let mut adapters: Vec<wgpu::Adapter> =
            gpui_kit::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
        if adapters.is_empty() {
            return Err(GpuUnavailable::NoAdapter);
        }
        let info = adapters.iter().map(|a| a.get_info()).collect::<Vec<_>>();
        let pick = {
            // 1) A discrete GPU (NVIDIA / AMD) beats everything else.
            let discrete = info
                .iter()
                .position(|i| i.device_type == wgpu::DeviceType::DiscreteGpu);
            // 2) Otherwise an integrated GPU (Intel Iris Xe etc).  Only when
            //    there is neither a discrete nor an integrated adapter do we
            //    fall back to a software rasteriser, which we then refuse.
            let integrated = info
                .iter()
                .position(|i| i.device_type == wgpu::DeviceType::IntegratedGpu);
            discrete.or(integrated).unwrap_or(0)
        };
        let adapter = adapters.swap_remove(pick);
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            // A software rasterizer is slower than the CPU path we already have.
            return Err(GpuUnavailable::Software(info.name));
        }
        let adapter_name = format!("{} · {:?}", info.name, info.backend);

        let (device, queue) = gpui_kit::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("trove-3d-device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }))
        .map_err(|error| GpuUnavailable::NoDevice(error.to_string()))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("trove-3d"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        // 4× MSAA where the adapter allows it, then 2×, then off. Both the
        // colour and the depth format have to accept the count.
        let samples = [4u32, 2]
            .into_iter()
            .find(|&count| {
                adapter
                    .get_texture_format_features(FORMAT)
                    .flags
                    .sample_count_supported(count)
                    && adapter
                        .get_texture_format_features(DEPTH_FORMAT)
                        .flags
                        .sample_count_supported(count)
            })
            .unwrap_or(1);
        let multisample = wgpu::MultisampleState {
            count: samples,
            ..Default::default()
        };

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("trove-3d"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE as u64),
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("trove-3d"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("trove-3d-uniforms"),
            size: UNIFORM_SIZE as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("trove-3d"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });

        let target = |format: wgpu::TextureFormat, blend| {
            Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })
        };
        // A pipeline used in a pass that has a depth attachment must declare
        // one too; the backdrop just neither writes nor tests against it,
        // which wgpu 29 spells as `None` for both fields.
        let depth_stencil = |write: bool| {
            Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(write),
                depth_compare: write.then_some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            })
        };

        let attributes = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
        let model_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("trove-3d-model"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_model"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: render3d::VertexData::STRIDE,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attributes,
                }],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // Shading is two-sided, so nothing is culled: open meshes and
                // inside-out exports both stay visible.
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: depth_stencil(true),
            multisample,
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_model"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[target(FORMAT, Some(wgpu::BlendState::REPLACE))],
            }),
            multiview_mask: None,
            cache: None,
        });

        // Points: the sprite corners come from the shader's `vertex_index`, so
        // the only vertex buffer holds one instance per point — position,
        // normal and the point's own colour.
        let point_attributes =
            wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x3];
        let point_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("trove-3d-points"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_point"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: render3d::PointData::STRIDE,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &point_attributes,
                }],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // Sprites turn to face the camera, but the pair of triangles
                // they are built from can wind either way on screen.
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: depth_stencil(true),
            multisample,
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_point"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[target(FORMAT, Some(wgpu::BlendState::REPLACE))],
            }),
            multiview_mask: None,
            cache: None,
        });

        let backdrop_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("trove-3d-backdrop"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_backdrop"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: depth_stencil(false),
            multisample,
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_backdrop"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[target(FORMAT, Some(wgpu::BlendState::REPLACE))],
            }),
            multiview_mask: None,
            cache: None,
        });

        Ok(Self {
            device,
            queue,
            model_pipeline,
            point_pipeline,
            backdrop_pipeline,
            uniforms,
            bind_group,
            samples,
            adapter: adapter_name,
        })
    }

    /// MSAA sample count the pipelines were built with (1 = none).
    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// Largest mesh the GPU upload is allowed to hold. Above this the
    /// viewport falls back to the CPU rasterizer, which renders at a
    /// bounded resolution regardless of how many triangles the model has.
    pub const GPU_UPLOAD_BUDGET: usize = 256 << 20;

    /// Move a mesh or cloud into GPU buffers, choosing indexed (smooth),
    /// expanded (flat) or instanced-point geometry exactly as the CPU path
    /// does. Returns `None` when the mesh is larger than
    /// [`GPU_UPLOAD_BUDGET`], so the caller can fall back to CPU.
    pub fn upload_capped(&self, mesh: &Mesh) -> Option<GpuMesh> {
        let estimated = Self::estimate_gpu_bytes(mesh);
        if estimated > Self::GPU_UPLOAD_BUDGET {
            return None;
        }
        Some(self.upload(mesh))
    }

    /// Bytes of GPU buffer a mesh will occupy: interleaved vertices plus
    /// the index list.
    fn estimate_gpu_bytes(mesh: &Mesh) -> usize {
        let vertex_bytes = mesh.vertex_count() as usize * render3d::VertexData::STRIDE as usize;
        let index_bytes = if mesh.has_vertex_normals() {
            mesh.triangle_count() * 3 * 4
        } else {
            // Flat-shaded: expanded per face, no index buffer.
            0
        };
        let point_bytes = if mesh.is_point_cloud() {
            mesh.vertex_count() as usize * render3d::PointData::STRIDE as usize
        } else {
            0
        };
        vertex_bytes + index_bytes + point_bytes
    }

    /// Move a mesh or cloud into GPU buffers, choosing indexed (smooth),
    /// expanded (flat) or instanced-point geometry exactly as the CPU path
    /// does.
    pub fn upload(&self, mesh: &Mesh) -> GpuMesh {
        if mesh.is_point_cloud() {
            let data = render3d::point_data(mesh);
            let points = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("trove-3d-points"),
                size: (data.bytes().len() as u64).max(4),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&points, 0, &data.bytes());
            return GpuMesh {
                vertices: points,
                indices: None,
                vertex_count: 0,
                index_count: 0,
                point_count: data.count,
            };
        }

        let data = render3d::vertex_data(mesh);
        let vertices = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("trove-3d-vertices"),
            size: (data.vertex_bytes().len() as u64).max(4),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&vertices, 0, &data.vertex_bytes());

        let indices = data.index_bytes().map(|bytes| {
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("trove-3d-indices"),
                size: (bytes.len() as u64).max(4),
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&buffer, 0, &bytes);
            buffer
        });

        GpuMesh {
            vertices,
            indices,
            vertex_count: data.vertex_count,
            index_count: data.indices.as_ref().map_or(0, |i| i.len() as u32),
            point_count: 0,
        }
    }

    /// Draw one frame and read it back as tightly packed BGRA, `width *
    /// height * 4` bytes — the same layout the CPU rasterizer produces, so the
    /// UI cannot tell the two apart. `None` when the read-back fails.
    pub fn render(&self, mesh: &GpuMesh, framing: &Framing, size: (u32, u32)) -> Option<Vec<u8>> {
        let (width, height) = (size.0.max(1), size.1.max(1));
        self.queue.write_buffer(
            &self.uniforms,
            0,
            &Uniforms::new(framing, (width, height)).to_bytes(),
        );
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let color = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("trove-3d-color"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        // With MSAA the pass draws into a multisampled target that resolves
        // into `color`; without it, straight into `color`.
        let msaa = (self.samples > 1).then(|| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("trove-3d-msaa"),
                size: extent,
                mip_level_count: 1,
                sample_count: self.samples,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
        });
        let msaa_view = msaa
            .as_ref()
            .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()));
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("trove-3d-depth"),
            size: extent,
            mip_level_count: 1,
            sample_count: self.samples,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("trove-3d-readback"),
            size: gpu::staging_len(width, height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("trove-3d"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("trove-3d"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: msaa_view.as_ref().unwrap_or(&color_view),
                    depth_slice: None,
                    // `Some` resolves the multisampled target into `color`.
                    resolve_target: msaa_view.as_ref().map(|_| &color_view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // Backdrop first: it covers the viewport and leaves depth alone.
            pass.set_pipeline(&self.backdrop_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);

            if mesh.point_count > 0 {
                // Six vertices per instance: the sprite's two triangles.
                pass.set_pipeline(&self.point_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.draw(0..6, 0..mesh.point_count);
            } else {
                pass.set_pipeline(&self.model_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                match &mesh.indices {
                    Some(indices) => {
                        pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
                        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                    }
                    None => {
                        pass.draw(0..mesh.vertex_count, 0..1);
                    }
                }
            }
        }

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &color,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(gpu::padded_row_bytes(width)),
                    rows_per_image: Some(height),
                },
            },
            extent,
        );
        self.queue.submit(Some(encoder.finish()));

        // The map only completes once the copy has run, so wait for the queue
        // and then for the callback.
        let slice = staging.slice(..);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        receiver.recv().ok()?.ok()?;

        let mapped = slice.get_mapped_range();
        let frame = gpu::unpack_bgra(&mapped, width, height);
        drop(mapped);
        staging.unmap();
        // Frees the textures and staging buffer this frame created.
        let _ = self.device.poll(wgpu::PollType::Poll);
        Some(frame)
    }
}

/// The GPU path cannot be exercised on a machine with no graphics device, so
/// the shader is checked the next best way: parsed and validated with the same
/// front end wgpu uses. This catches WGSL syntax errors, type errors and — the
/// failure that would otherwise be silent until a frame looked wrong — a
/// uniform block whose byte layout disagrees with [`Uniforms`].
#[cfg(test)]
mod tests {
    use super::*;
    use naga::valid::{Capabilities, ValidationFlags, Validator};

    /// Parse and validate the shader, panicking with the diagnostic on error.
    fn module() -> naga::Module {
        let module = naga::front::wgsl::parse_str(SHADER).unwrap_or_else(|error| {
            panic!(
                "gpu3d.wgsl does not parse:\n{}",
                error.emit_to_string(SHADER)
            )
        });
        Validator::new(ValidationFlags::all(), Capabilities::empty())
            .validate(&module)
            .unwrap_or_else(|error| {
                panic!(
                    "gpu3d.wgsl does not validate:\n{}",
                    error.emit_to_string(SHADER)
                )
            });
        module
    }

    /// Every entry point `GpuRenderer::new` asks for, and nothing else.
    #[test]
    fn the_shader_declares_the_entry_points_the_pipelines_use() {
        let module = module();
        let mut found: Vec<(&str, naga::ShaderStage)> = module
            .entry_points
            .iter()
            .map(|entry| (entry.name.as_str(), entry.stage))
            .collect();
        found.sort_by_key(|(name, _)| *name);
        assert_eq!(
            found,
            vec![
                ("fs_backdrop", naga::ShaderStage::Fragment),
                ("fs_model", naga::ShaderStage::Fragment),
                ("fs_point", naga::ShaderStage::Fragment),
                ("vs_backdrop", naga::ShaderStage::Vertex),
                ("vs_model", naga::ShaderStage::Vertex),
                ("vs_point", naga::ShaderStage::Vertex),
            ]
        );
    }

    /// `vs_model` reads position and normal, `vs_point` also reads the point's
    /// colour — one `vec3<f32>` per location its pipeline's vertex buffer
    /// layout describes, in the same order and with the same stride.
    #[test]
    fn the_vertex_inputs_match_the_buffer_layouts() {
        let module = module();
        for (entry_point, locations, stride) in [
            ("vs_model", 2, render3d::VertexData::STRIDE),
            ("vs_point", 3, render3d::PointData::STRIDE),
        ] {
            let entry = module
                .entry_points
                .iter()
                .find(|entry| entry.name == entry_point)
                .unwrap_or_else(|| panic!("{entry_point} present"));

            let mut inputs: Vec<(u32, naga::ScalarKind, naga::VectorSize)> = Vec::new();
            for argument in &entry.function.arguments {
                let Some(naga::Binding::Location { location, .. }) = argument.binding else {
                    continue;
                };
                let naga::TypeInner::Vector { size, scalar } = module.types[argument.ty].inner
                else {
                    panic!("{entry_point} input @location({location}) is not a vector");
                };
                inputs.push((location, scalar.kind, size));
            }
            inputs.sort_by_key(|(location, _, _)| *location);
            let expected: Vec<_> = (0..locations)
                .map(|location| {
                    (
                        location as u32,
                        naga::ScalarKind::Float,
                        naga::VectorSize::Tri,
                    )
                })
                .collect();
            assert_eq!(inputs, expected, "{entry_point} inputs");
            // `locations` vec3<f32> per vertex or instance: the declared stride.
            assert_eq!(stride, locations * 3 * 4, "{entry_point} stride");
        }
    }

    /// The heart of it: the byte offsets WGSL assigns to the uniform struct
    /// must equal the offsets `Uniforms::to_bytes` writes to. A mismatch here
    /// produces a picture that is subtly or catastrophically wrong with no
    /// other symptom.
    #[test]
    fn the_uniform_block_layout_matches_the_rust_side() {
        let module = module();
        let layout = uniform_layout(&module);

        // (name, bytes) in declaration order — the Rust packing order.
        let expected: [(&str, u32); 9] = [
            ("view_proj", 64),
            ("light", 16),
            ("material", 16),
            ("params", 16),
            ("params2", 16),
            ("eye", 16),
            ("viewport", 16),
            ("bg_top", 16),
            ("bg_bottom", 16),
        ];

        assert_eq!(layout.len(), expected.len(), "member count");
        let mut offset = 0;
        for (member, (name, size)) in layout.iter().zip(expected) {
            assert_eq!(member.name.as_deref(), Some(name), "member at {offset}");
            assert_eq!(member.offset, offset, "{name} offset");
            offset += size;
        }
        assert_eq!(offset as usize, UNIFORM_SIZE, "total block size");

        let declared = struct_size(&module, layout_type(&module));
        assert_eq!(
            declared, UNIFORM_SIZE as u32,
            "WGSL struct size must equal UNIFORM_SIZE"
        );
    }

    /// 16-byte alignment is what makes the flat float packing in
    /// `Uniforms::to_bytes` legal; assert the shader's own view of it.
    #[test]
    fn the_uniform_block_is_sixteen_byte_aligned() {
        let module = module();
        let mut layouter = naga::proc::Layouter::default();
        layouter
            .update(module.to_ctx())
            .expect("lay out the shader");

        let laid_out = &layouter[layout_type(&module)];
        assert_eq!(laid_out.alignment, naga::proc::Alignment::SIXTEEN);
        assert_eq!(laid_out.size, UNIFORM_SIZE as u32);
    }

    /// The members of the shader's `Uniforms` struct, in order.
    fn uniform_layout(module: &naga::Module) -> &[naga::StructMember] {
        let naga::TypeInner::Struct { members, .. } = &module.types[layout_type(module)].inner
        else {
            panic!("the uniform binding is not a struct");
        };
        members
    }

    /// Handle of the `@group(0) @binding(0)` uniform's type.
    fn layout_type(module: &naga::Module) -> naga::Handle<naga::Type> {
        module
            .global_variables
            .iter()
            .find(|(_, variable)| {
                variable.space == naga::AddressSpace::Uniform
                    && variable
                        .binding
                        .is_some_and(|binding| (binding.group, binding.binding) == (0, 0))
            })
            .map(|(_, variable)| variable.ty)
            .expect("the shader declares a uniform at @group(0) @binding(0)")
    }

    /// Size of a type as the shader lays it out.
    fn struct_size(module: &naga::Module, ty: naga::Handle<naga::Type>) -> u32 {
        let mut layouter = naga::proc::Layouter::default();
        layouter
            .update(module.to_ctx())
            .expect("lay out the shader");
        layouter[ty].size
    }
}
