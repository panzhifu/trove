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

use std::sync::{Arc, OnceLock, mpsc};
use trove_core::media::formats::meshlet::{self, Meshlet};
use trove_core::media::formats::types::{
    Mesh, NO_TEXTURE, TextureMap, Winding, mip_chain, resize_rgba,
};
use trove_core::media::gpu::{self, UNIFORM_SIZE, Uniforms};
use trove_core::media::height_color::HeightUniforms;
use trove_core::media::render3d::{self, Framing};

/// Colour formats this renderer can use, in order of preference.
///
/// `Bgra8Unorm` first: it is what gpui's images hold, so a frame read back from
/// it needs no per-pixel swizzle. Both are 8-bit unorm, so the shader writes
/// the same perceptual values either way — the CPU path's output is matched by
/// construction.
const COLOR_FORMATS: [wgpu::TextureFormat; 2] = [
    wgpu::TextureFormat::Bgra8Unorm,
    wgpu::TextureFormat::Rgba8Unorm,
];
/// Depth buffer for the model pass.
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Cap on a model's texture array, mip chains included.
///
/// Every layer is one RGBA8 square of the set's largest edge — 2048² plus its
/// chain is ~22 MiB — so a scene of hundreds of maps asks for gigabytes and the
/// driver answers `OutOfMemory`. Keeping the whole set under this turns a large
/// texture set into a degraded preview (a smaller shared edge, or the flat
/// material when even that will not fit) instead of a crash.
const MODEL_TEXTURE_BUDGET: u64 = 256 << 20;

/// Bytes a `layers`-deep, `dim`-square RGBA8 array costs, mip chain included
/// (a chain is ~4/3 of its base level).
fn array_bytes(dim: u32, layers: u64) -> u64 {
    layers * (dim as u64 * dim as u64 * 4) * 4 / 3
}

/// The shared edge and mip count a model's texture array is created with, or
/// `None` when even the smallest useful edge overflows [`MODEL_TEXTURE_BUDGET`].
///
/// An array demands one size for every layer, so the edge is the largest map's,
/// halved until the whole set fits. Below a sane floor and it is not worth a
/// texture at all — the caller binds the white stand-in and the model opens
/// flat rather than not at all.
fn texture_layout(maps: &[TextureMap]) -> Option<(u32, u32)> {
    let mut dim = maps
        .iter()
        .map(|m| m.width.max(m.height))
        .max()
        .unwrap_or(1)
        .clamp(1, 2048);
    let layers = maps.len() as u64 + 1;
    while dim > 16 && array_bytes(dim, layers) > MODEL_TEXTURE_BUDGET {
        dim /= 2;
    }
    (array_bytes(dim, layers) <= MODEL_TEXTURE_BUDGET).then(|| (dim, 32 - dim.leading_zeros()))
}

/// The read-back order of a frame rendered in `format`, for [`gpu::unpack_bgra`].
fn pixel_order(format: wgpu::TextureFormat) -> gpu::PixelOrder {
    match format {
        wgpu::TextureFormat::Bgra8Unorm => gpu::PixelOrder::Bgra,
        _ => gpu::PixelOrder::Rgba,
    }
}

/// Choose the colour format and the MSAA sample count together.
///
/// Multisampling is where a model's silhouette aliases worst, and the BGRA
/// format saves a swizzle on every interactive frame — but neither is worth
/// losing the renderer over, so the order is: the preferred format with MSAA,
/// the other format with MSAA, and finally the preferred format without.
fn choose_format_and_samples(adapter: &wgpu::Adapter) -> (wgpu::TextureFormat, u32) {
    let supports = |format: wgpu::TextureFormat, count: u32| {
        adapter
            .get_texture_format_features(format)
            .flags
            .sample_count_supported(count)
            && adapter
                .get_texture_format_features(DEPTH_FORMAT)
                .flags
                .sample_count_supported(count)
    };
    let best =
        |format: wgpu::TextureFormat| [4u32, 2].into_iter().find(|&count| supports(format, count));

    let [preferred, fallback] = COLOR_FORMATS;
    match (best(preferred), best(fallback)) {
        (Some(count), _) => (preferred, count),
        (None, Some(count)) => (fallback, count),
        (None, None) => (preferred, 1),
    }
}

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
    /// The device came up but could not hold this model: a buffer or the
    /// packed texture array asked for more than the driver had. Carries the
    /// backend's own message. The viewport answers this by falling back to the
    /// CPU rasteriser, so a model too large for the GPU still opens.
    OutOfMemory(String),
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
    /// The mesh is a closed, consistently wound surface, so its back faces are
    /// hidden by its front ones and can be culled instead of shaded. Open
    /// shells and inside-out files keep both faces.
    cull_backfaces: bool,
    /// Spatial clusters of triangles, when the mesh was large enough to be
    /// partitioned. Each is one draw call the frustum can skip; empty means
    /// "draw the whole index buffer in one call".
    meshlets: Vec<Meshlet>,
    /// Per-vertex RGB, present only when the file carried its own colours —
    /// a glTF base colour factor or an OBJ material's diffuse. `Some` puts the
    /// mesh on the coloured pipelines, which read this as a second vertex
    /// buffer; `None` takes the flat material from the uniform block instead.
    colors: Option<wgpu::Buffer>,
    /// Per-vertex `[u, v, layer]`, `Some` exactly when `colors` is — the
    /// coloured pipelines declare the attribute, so an untextured mesh binds
    /// a buffer of zeros pointing at the white layer.
    tex: Option<wgpu::Buffer>,
    /// This mesh's textures as one array, bound alongside the shared
    /// uniforms; built at upload, white-filled when the mesh has none.
    bind_group: wgpu::BindGroup,
}

impl GpuMesh {
    /// Index ranges to draw for `framing`, or `None` for the whole buffer in
    /// one call.
    ///
    /// `None` when the mesh was not partitioned, and when every cluster is
    /// visible — one draw beats thousands when the culling has nothing to
    /// skip. An empty `Some` means nothing is on screen at all.
    fn visible_meshlets(&self, framing: &Framing) -> Option<Vec<std::ops::Range<u32>>> {
        if self.meshlets.is_empty() {
            return None;
        }
        // Debug escape hatch: draw every cluster, cull nothing.
        if std::env::var_os("TROVE_DEBUG_NO_CULL").is_some() {
            return None;
        }
        let frustum = framing.frustum();
        let ranges: Vec<std::ops::Range<u32>> = self
            .meshlets
            .iter()
            .filter(|meshlet| frustum.intersects_bounds(&meshlet.bounds))
            .map(Meshlet::index_range)
            .collect();
        (ranges.len() != self.meshlets.len()).then_some(ranges)
    }
}

/// Off-screen targets for one viewport size, reused across frames.
///
/// A 1080p viewport with 4× MSAA is roughly 40 MB of textures plus an 8 MB
/// read-back buffer; allocating and freeing that every frame is driver churn
/// the preview does not need, and the camera moves often enough that it would
/// happen many times a second.
struct Targets {
    width: u32,
    height: u32,
    /// MSAA count the textures were created with; the pipeline has to match.
    samples: u32,
    /// Resolve target, and the copy source.
    color: wgpu::Texture,
    color_view: wgpu::TextureView,
    /// Multisampled target the pass draws into, when MSAA is on.
    msaa: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// Held (not read) so the depth texture outlives the view of it. It is
    /// also sampled by the post pass, which is why it outlives the view.
    _depth: wgpu::Texture,
    depth_view: wgpu::TextureView,
    /// Where the eye-dome lighting pass writes, and what the read-back copies
    /// when it ran. `None` on a single-sampled frame, where there is no
    /// multisampled depth for the post pass to read.
    edl_color: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// The depth and resolved colour the post pass reads. `None` with it.
    edl_bind_group: Option<wgpu::BindGroup>,
    staging: wgpu::Buffer,
}

/// The draw pipelines for one multisample count.
///
/// Model pipelines come in four shapes, because two decisions are per-mesh:
/// back-face culling (a closed surface hides its own back faces, an open
/// shell does not) and colour (a mesh carrying its own per-vertex materials
/// reads a second vertex buffer; a plain one takes the flat material from the
/// uniform block). The point and backdrop pipelines are unaffected.
struct Pipelines {
    /// Model pipeline for a closed, outward-wound mesh.
    model_culled: wgpu::RenderPipeline,
    /// Model pipeline that draws both faces.
    model_two_sided: wgpu::RenderPipeline,
    /// Same two for a mesh whose vertices carry their own colours.
    model_colored_culled: wgpu::RenderPipeline,
    model_colored_two_sided: wgpu::RenderPipeline,
    point: wgpu::RenderPipeline,
    backdrop: wgpu::RenderPipeline,
}

/// Device, pipelines and the resources shared by every frame.
/// Pack one f32 channel of an RGBA16Float texel: scene-linear radiance has
/// no subnormals worth keeping, so the conversion clamps negatives to zero
/// and everything past half's range to infinity.
fn f16_bits(value: f32) -> u16 {
    let value = value.max(0.0);
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x007f_ffff;
    if exponent >= 0x1f {
        return sign | 0x7c00;
    }
    if exponent <= 0 {
        return sign;
    }
    sign | ((exponent as u16) << 10) | ((mantissa >> 13) as u16)
}

pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Pipelines for a settled frame, at the adapter's best sample count.
    pipelines: Pipelines,
    /// Single-sampled pipelines for frames drawn while the pointer is down.
    /// `None` when the adapter cannot multisample at all, in which case
    /// `pipelines` is already single-sampled.
    interactive_pipelines: Option<Pipelines>,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// The uniform+texture+sampler layout every draw pipeline shares; kept
    /// so a mesh's own texture bind group can be built at upload time.
    bind_group_layout: wgpu::BindGroupLayout,
    texture_sampler: wgpu::Sampler,
    /// The baked studio environment: one prefiltered cube shared by every
    /// mesh bind group, so the specular's IBL term reads the same room the
    /// thumbnails' analytic approximation stands in for.
    env_view: wgpu::TextureView,
    /// Layout of the post pass's depth + resolved-colour bindings. Separate
    /// from the uniform-only layout the draw pipelines use, because only the
    /// post pass reads those two textures.
    edl_bind_group_layout: wgpu::BindGroupLayout,
    /// Eye-dome lighting and gap filling over a settled point-cloud frame.
    /// The CPU rasteriser's `render3d::enhance_points`, as a full-screen pass.
    edl_pipeline: wgpu::RenderPipeline,
    /// The key light's shadow map and the sampler that compares against it.
    /// The texture is rewritten by the shadow pass every frame that draws a
    /// mesh, and read by every model fragment; a point-cloud frame never
    /// writes it, so a cloud never shadows.
    _shadow_depth: wgpu::Texture,
    shadow_view: wgpu::TextureView,
    shadow_cmp: wgpu::Sampler,
    /// The shadow pass: the mesh from the light, depth only.
    shadow_pipeline: wgpu::RenderPipeline,
    /// The shadow pass's uniforms-only group, kept apart from the full one
    /// for the usage-conflict reason its layout documents.
    _shadow_layout: wgpu::BindGroupLayout,
    _shadow_pipeline_layout: wgpu::PipelineLayout,
    shadow_bind_group: wgpu::BindGroup,
    /// The split-sum environment BRDF table and its clamped sampler, bound
    /// beside the environment cube in every model bind group.
    _brdf_texture: wgpu::Texture,
    brdf_view: wgpu::TextureView,
    lut_sampler: wgpu::Sampler,
    /// Colour format every target and pipeline uses, chosen so the read-back
    /// needs no swizzle where the adapter allows it.
    format: wgpu::TextureFormat,
    /// MSAA sample count of `pipelines`; 1 when the adapter cannot
    /// multisample. A model's silhouette is where aliasing shows worst, so
    /// this is worth asking for.
    samples: u32,
    /// Current off-screen targets, rebuilt only when the viewport is resized.
    targets: std::sync::Mutex<Option<Targets>>,
    /// Adapter description, shown in the status line.
    pub adapter: String,
}

/// What the chosen device supports, in the terms this renderer cares about.
///
/// Printed once at start-up: on a machine whose GPU is not what it looks like
/// (a software rasteriser, no MSAA, no 64-bit atomics) this is the one place
/// that says so, and the next stage of the large-model work reads
/// `int64_atomics` to choose between its packed-buffer and fallback paths.
#[derive(Debug, Clone)]
pub struct DeviceCaps {
    /// 64-bit `atomicMin`/`atomicMax` in shaders, which is what a packed
    /// depth-and-attribute buffer needs (Nimbus leans on NVIDIA's
    /// `atomic_int64` extension for exactly that trick). This is what the
    /// *adapter offers*: the device requests it when a path needs it, and
    /// without it that path has to fall back to a 32-bit depth buffer plus a
    /// resolve pass over every point.
    pub int64_atomics: bool,
    /// Largest storage-buffer binding, which bounds how many points can be
    /// resident in one buffer.
    pub max_storage_buffer: u64,
    /// Largest single buffer allocation.
    pub max_buffer: u64,
    /// MSAA count settled frames are drawn with.
    pub samples: u32,
    /// Colour format of every target.
    pub format: wgpu::TextureFormat,
    /// Adapter kind, so a software or integrated device is visible here.
    pub device_type: wgpu::DeviceType,
}

impl DeviceCaps {
    /// One line for the status line.
    pub fn summary(&self) -> String {
        format!(
            "{} · {}× MSAA · {:?} · storage {} MiB · buffer {} MiB · u64 atomics {}",
            match self.device_type {
                wgpu::DeviceType::DiscreteGpu => "discrete GPU",
                wgpu::DeviceType::IntegratedGpu => "integrated GPU",
                wgpu::DeviceType::VirtualGpu => "virtual GPU",
                wgpu::DeviceType::Cpu => "software",
                wgpu::DeviceType::Other => "other",
            },
            self.samples,
            self.format,
            self.max_storage_buffer >> 20,
            self.max_buffer >> 20,
            if self.int64_atomics { "yes" } else { "no" },
        )
    }
}

/// The one GPU renderer for the whole process, brought up on first use and
/// shared by every model viewport after that.
///
/// Bringing a device up — instance, adapter enumeration, pipelines — costs
/// hundreds of milliseconds, and a small model spends nearly all of its load
/// time there: its geometry parses in milliseconds and uploads in fewer.
/// Sharing one device turns "every preview pays for the GPU" into "the first
/// one does", which is most of what makes a small model feel instant.
///
/// Safe to share because the renderer is stateless between frames — the
/// off-screen targets follow whoever renders next — and only one model
/// viewport is open at a time. A failure is cached too: an adapter probe that
/// found nothing will find nothing next time, so the search is paid for once.
pub(crate) fn shared_renderer() -> Result<Arc<GpuRenderer>, GpuUnavailable> {
    static SHARED: OnceLock<Result<Arc<GpuRenderer>, GpuUnavailable>> = OnceLock::new();
    SHARED
        .get_or_init(|| GpuRenderer::new().map(Arc::new))
        .clone()
}

impl GpuRenderer {
    /// Bring up a device, or explain why not. Blocking: callers run this off
    /// the UI thread.
    pub fn new() -> Result<Self, GpuUnavailable> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            // Every backend the platform can offer, not just Vulkan/GL: the
            // instance is what decides which adapters exist at all, so asking
            // for Vulkan and GL alone means no Metal on macOS and no D3D12 on
            // Windows — the viewport would silently render on the CPU there.
            //
            // GL is deliberately *not* asked for. It is the one backend that
            // goes through EGL on X11, and probing it costs a display
            // initialisation that fails noisily on machines whose Mesa has no
            // driver for the GPU (`failed to create dri2 screen`); the only
            // adapter it could add is a software rasteriser, which this
            // renderer refuses anyway (see `GpuUnavailable::Software`).
            backends: wgpu::Backends::PRIMARY,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });

        // On Optimus / hybrid-graphics laptops `request_adapter` with a
        // `HighPerformance` hint can still hand us the integrated GPU, so
        // enumerate every adapter and pick the discrete one. The fallback
        // chain: discrete → non-software → whatever is there.
        let adapters: Vec<wgpu::Adapter> =
            gpui_kit::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
        if adapters.is_empty() {
            // Worth printing: "no adapter" is the answer to "why is this
            // rendering on the CPU", and it is not visible anywhere else.
            eprintln!(
                "trove: no graphics adapter found on Vulkan/D3D12/Metal — on Linux this \
                 usually means no working Vulkan driver (`vulkaninfo --summary` says so)"
            );
            return Err(GpuUnavailable::NoAdapter);
        }
        let info = adapters.iter().map(|a| a.get_info()).collect::<Vec<_>>();
        // The list, every time: "there is a GPU in this machine" and "the
        // renderer can use it" are different statements, and this is the line
        // that tells them apart.
        for adapter in &info {
            eprintln!(
                "trove: adapter found: {} · {:?} · {:?} · driver {} {}",
                adapter.name,
                adapter.device_type,
                adapter.backend,
                adapter.driver,
                adapter.driver_info
            );
        }
        // Preference order: a discrete GPU (NVIDIA / AMD) beats an integrated
        // one, which beats anything else; a software rasteriser is last and is
        // refused, because the built-in CPU rasteriser is faster for a preview.
        let rank = |info: &wgpu::AdapterInfo| match info.device_type {
            wgpu::DeviceType::DiscreteGpu => 0,
            wgpu::DeviceType::IntegratedGpu => 1,
            wgpu::DeviceType::VirtualGpu => 2,
            wgpu::DeviceType::Other => 3,
            wgpu::DeviceType::Cpu => 4,
        };
        let mut order: Vec<usize> = (0..adapters.len()).collect();
        order.sort_by_key(|index| rank(&info[*index]));

        // Every candidate is *tried*, not just ranked. An adapter that cannot
        // produce a device — a hybrid laptop whose discrete node is half
        // installed, say — must not cost the user the one that works.
        let mut software = None;
        let mut last_error = None;
        let mut chosen = None;
        for index in order {
            let adapter = &adapters[index];
            let candidate = adapter.get_info();
            if candidate.device_type == wgpu::DeviceType::Cpu {
                eprintln!(
                    "trove: skipping the software rasteriser {} — the built-in CPU \
                     rasteriser is faster for a preview",
                    candidate.name
                );
                software.get_or_insert(candidate.name);
                continue;
            }
            // Decided per candidate, from what that adapter can do.
            let formats = choose_format_and_samples(adapter);
            match gpui_kit::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("trove-3d-device"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            })) {
                Ok((device, queue)) => {
                    chosen = Some((device, queue, candidate, adapter.features(), formats));
                    break;
                }
                Err(error) => {
                    eprintln!(
                        "trove: no device from {} ({error}); trying the next adapter",
                        candidate.name
                    );
                    last_error = Some(error.to_string());
                }
            }
        }
        let Some((device, queue, info, adapter_features, (format, samples))) = chosen else {
            // Only software was on offer, or nothing could be created at all.
            return Err(match (software, last_error) {
                (Some(name), _) => GpuUnavailable::Software(name),
                (None, Some(detail)) => GpuUnavailable::NoDevice(detail),
                (None, None) => GpuUnavailable::NoAdapter,
            });
        };
        let adapter_name = format!("{} · {:?}", info.name, info.backend);

        // An error that no error scope captures would otherwise reach wgpu's
        // default handler, which panics the worker thread the driver reports
        // on — and that panic tears the process down, leaving any task waiting
        // on a frame to be "polled after completion". Out-of-memory is the one
        // that actually happens on a laptop GPU, so log it and let the upload's
        // own error scope (or the CPU fallback) decide; never abort the
        // process over a resource the driver could not hand out.
        device.on_uncaptured_error(std::sync::Arc::new(|error: wgpu::Error| {
            tracing::error!(error = ?error, "wgpu device error");
        }));

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("trove-3d"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("trove-3d"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE as u64),
                    },
                    count: None,
                },
                // The model's base-colour textures, as one array a vertex's
                // slot names into. Pipelines that do not sample — the plain
                // material, the points — still bind this group; the extra
                // entries simply go unread.
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // The studio environment, prefiltered into five mips: the
                // specular's IBL term, sampled along the reflection.
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::Cube,
                        multisampled: false,
                    },
                    count: None,
                },
                // The key light's shadow map and its comparison sampler: the
                // model fragments test themselves against it. Pipelines that
                // never test — the backdrop, the points — still bind this
                // group; the entries go unread, like the texture array's.
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
                // The split-sum environment BRDF table, baked once by the
                // CPU and shared by every bind group, with its own clamped
                // sampler — the texture sampler repeats, and a BRDF table
                // must not wrap at its edges.
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
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
        // A 1×1 white texture stands in wherever nothing is sampled: a mesh
        // without textures binds it, and "multiply by white" is "do nothing".
        let white = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("trove-3d-white"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &white,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let white_view = white.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        // The studio environment: baked once on the host (the sharp room plus
        // EEVEE's prefilter cascade), uploaded as a float cube so a mirror
        // still reads the panels at their real brightness.
        let env_mips = render3d::environment_mips();
        let env = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("trove-3d-env"),
            size: wgpu::Extent3d {
                width: env_mips[0].face_size,
                height: env_mips[0].face_size,
                depth_or_array_layers: 6,
            },
            mip_level_count: env_mips.len() as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level, mip) in env_mips.iter().enumerate() {
            let bytes: Vec<u8> = mip
                .faces
                .iter()
                .flat_map(|texel| {
                    texel
                        .map(f16_bits)
                        .into_iter()
                        .flat_map(|h| h.to_le_bytes())
                })
                .collect();
            let texel_bytes = 2u32;
            let face = mip.face_size;
            for layer in 0..6u32 {
                let face_bytes = (face * face * 4 * texel_bytes) as usize;
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &env,
                        mip_level: level as u32,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: 0,
                            z: layer,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &bytes[layer as usize * face_bytes..(layer as usize + 1) * face_bytes],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(face * 4 * texel_bytes),
                        rows_per_image: Some(face),
                    },
                    wgpu::Extent3d {
                        width: face,
                        height: face,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        let env_view = env.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::Cube),
            ..Default::default()
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("trove-3d-texture"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // Trilinear across the mip chain, with 16-tap anisotropy: the two
            // settings that keep a minified texture from shimmering on a
            // surface seen at a grazing angle — the ones Blender's viewport
            // turns on for its image textures. The env cube is sampled at an
            // explicit level and does not read either, and the address modes
            // stay glTF's repeat default — the CPU sampler has always
            // answered with `fract`, and a clamped edge streaks every model
            // whose UVs step outside the unit square.
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 16,
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            ..Default::default()
        });
        // The key light's shadow map: a fixed-size depth texture, rewritten
        // by the depth-only pass every frame draws a mesh. Reversed-Z like
        // the main pass — the clear value is the far end — and sampled
        // through a comparison sampler so the PCF's nine taps each come back
        // hardware-filtered: bilinear on the compare result, the softness
        // nine plain taps would need thirty-six for.
        let shadow_depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("trove-3d-shadow"),
            size: wgpu::Extent3d {
                width: render3d::SHADOW_MAP_SIZE,
                height: render3d::SHADOW_MAP_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                // The debug dump below reads the map back onto the CPU.
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let shadow_view = shadow_depth.create_view(&wgpu::TextureViewDescriptor::default());
        let shadow_cmp = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("trove-3d-shadow-compare"),
            // Linear filtering on a comparison sampler is what turns each tap
            // into a bilinear PCF sample; `GreaterEqual` is "reference at or
            // in front of the stored depth", the direction reversed-Z calls
            // lit.
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::GreaterEqual),
            ..Default::default()
        });
        // The split-sum environment BRDF table, baked once by the CPU and
        // uploaded as half-float pairs — the same table `render3d`'s
        // specular reads bilinearly, so a thumbnail's reflections and the
        // viewport's come off one set of numbers. Two channels: u = NoV,
        // v = roughness, rg = (scale, bias).
        let brdf = render3d::environment_brdf_lut();
        let brdf_bytes: Vec<u8> = brdf
            .data
            .iter()
            .flat_map(|pair| {
                let mut bytes = Vec::with_capacity(4);
                for value in pair {
                    bytes.extend_from_slice(&f16_bits(*value).to_le_bytes());
                }
                bytes
            })
            .collect();
        let brdf_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("trove-3d-brdf"),
            size: wgpu::Extent3d {
                width: brdf.size,
                height: brdf.size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &brdf_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &brdf_bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(brdf.size * 4),
                rows_per_image: Some(brdf.size),
            },
            wgpu::Extent3d {
                width: brdf.size,
                height: brdf.size,
                depth_or_array_layers: 1,
            },
        );
        let brdf_view = brdf_texture.create_view(&wgpu::TextureViewDescriptor::default());
        // The table must not wrap: its edges are the domain's ends (NoV 0..1,
        // roughness 0..1), and a repeating sampler would blend across them.
        let lut_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("trove-3d-brdf"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("trove-3d-default"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&white_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&env_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&shadow_view),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Sampler(&shadow_cmp),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&brdf_view),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::Sampler(&lut_sampler),
                },
            ],
        });

        // The point-cloud post pass reads the multisampled depth attachment
        // and the resolved colour, so it needs a layout of its own. It is
        // single-sampled and has no depth attachment of its own: it only ever
        // runs on the settled, multisampled frame.
        let edl_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("trove-3d-edl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Depth,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: true,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                ],
            });
        let edl_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("trove-3d-edl"),
            bind_group_layouts: &[Some(&layout), Some(&edl_bind_group_layout)],
            immediate_size: 0,
        });
        let edl_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("trove-3d-edl"),
            layout: Some(&edl_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_backdrop"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_edl"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        // The shadow pass reads nothing but the uniforms — binding the full
        // group here would hold the shadow map as a resource in the very
        // pass that writes it, which wgpu refuses as a usage conflict — so
        // the pass carries a uniforms-only layout and bind group of its own.
        let shadow_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("trove-3d-shadow"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE as u64),
                },
                count: None,
            }],
        });
        let shadow_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("trove-3d-shadow"),
                bind_group_layouts: &[Some(&shadow_layout)],
                immediate_size: 0,
            });
        let shadow_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("trove-3d-shadow"),
            layout: &shadow_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });

        // The shadow pass: the mesh seen from the key light, depth only. One
        // pipeline for every sample count — the shadow map is never
        // multisampled — and no fragment stage, since the pipeline writes
        // depth and nothing else. Back faces are kept rather than culled: a
        // thin shell seen edge-on by the light must still occlude, and the
        // bias handles the self-shadowing that keeping both faces invites.
        let shadow_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("trove-3d-shadow"),
            layout: Some(&shadow_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_shadow"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: render3d::VertexData::STRIDE,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    // Position only: the interleaved normal rides along
                    // unread.
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                }],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                // Reversed-Z, like the main pass: nearer to the light is
                // larger, the clear is zero, and GreaterEqual keeps the
                // nearest surface per texel.
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: None,
            multiview_mask: None,
            cache: None,
        });

        // Built once per sample count: the settled frame gets the adapter's
        // MSAA, the frame drawn while the pointer is down gets none — it is
        // about to be scaled up anyway, so its antialiasing is spent on pixels
        // nobody sees.
        let build = |sample_count: u32| -> Pipelines {
            let multisample = wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            };
            let target = |format: wgpu::TextureFormat, blend| {
                Some(wgpu::ColorTargetState {
                    format,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })
            };
            // A pipeline used in a pass that has a depth attachment must
            // declare one too; the backdrop just neither writes nor tests
            // against it, which wgpu 29 spells as `None` for both fields.
            let depth_stencil = |write: bool| {
                Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(write),
                    // `LessEqual`, not `Less`: coincident surfaces — a screen quad laid
                    // exactly over its backing panel, a common cheat in game
                    // assets — then resolve by draw order, the same tie-break the
                    // CPU rasteriser's `<=` test gives, instead of per-pixel
                    // speckle.
                    // Reversed-Z: the depth grows towards the camera
                    // (`near / vz`, `1` at the near plane), so nearer
                    // fragments carry larger values and the compare flips
                    // with them. Coplanar surfaces resolve by draw order —
                    // the CPU rasteriser's own `<=` tie-break, mirrored.
                    depth_compare: write.then_some(wgpu::CompareFunction::GreaterEqual),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                })
            };

            let attributes = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
            let color_attributes = wgpu::vertex_attr_array![2 => Float32x3];
            let tex_attributes = wgpu::vertex_attr_array![
                3 => Float32x4,
                4 => Float32x4,
                5 => Float32x2,
                6 => Float32x4,
                7 => Float32x4
            ];
            let model = |cull: bool, colored: bool| {
                // A coloured mesh reads its RGB from a second vertex buffer and
                // its texture coordinate from a third, so the interleaved
                // position+normal stride the plain pipelines use stays
                // untouched.
                let buffers = if colored {
                    &[
                        wgpu::VertexBufferLayout {
                            array_stride: render3d::VertexData::STRIDE,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &attributes,
                        },
                        wgpu::VertexBufferLayout {
                            array_stride: 3 * 4,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &color_attributes,
                        },
                        wgpu::VertexBufferLayout {
                            array_stride: render3d::VertexData::TEX_STRIDE,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &tex_attributes,
                        },
                    ][..]
                } else {
                    &[wgpu::VertexBufferLayout {
                        array_stride: render3d::VertexData::STRIDE,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &attributes,
                    }][..]
                };
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(match (colored, cull) {
                        (true, true) => "trove-3d-model-colored-culled",
                        (true, false) => "trove-3d-model-colored",
                        (false, true) => "trove-3d-model-culled",
                        (false, false) => "trove-3d-model",
                    }),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some(if colored {
                            "vs_model_colored"
                        } else {
                            "vs_model"
                        }),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers,
                    },
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        strip_index_format: None,
                        front_face: wgpu::FrontFace::Ccw,
                        // Culling is the GPU's own cheap pass: it discards a
                        // triangle before it is shaded. Only a closed mesh
                        // may use it — an open shell's back face is the
                        // surface you see when you look at its other side,
                        // and an inside-out file's front faces are its
                        // inside. Both are drawn two-sided instead.
                        cull_mode: cull.then_some(wgpu::Face::Back),
                        unclipped_depth: false,
                        polygon_mode: wgpu::PolygonMode::Fill,
                        conservative: false,
                    },
                    depth_stencil: depth_stencil(true),
                    multisample,
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(if colored {
                            "fs_model_textured"
                        } else {
                            "fs_model"
                        }),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        targets: &[target(format, Some(wgpu::BlendState::REPLACE))],
                    }),
                    multiview_mask: None,
                    cache: None,
                })
            };
            let model_culled = model(true, false);
            let model_two_sided = model(false, false);
            let model_colored_culled = model(true, true);
            let model_colored_two_sided = model(false, true);

            // Points: the sprite corners come from the shader's
            // `vertex_index`, so the only vertex buffer holds one instance per
            // point — position, normal, the point's own colour, and the two
            // scalar channels a field is read from.
            let point_attributes = wgpu::vertex_attr_array![
                0 => Float32x3,
                1 => Float32x3,
                2 => Float32x3,
                3 => Float32,
                4 => Float32
            ];
            let point = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                    // Sprites turn to face the camera, but the pair of
                    // triangles they are built from can wind either way on
                    // screen.
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
                    targets: &[target(format, Some(wgpu::BlendState::REPLACE))],
                }),
                multiview_mask: None,
                cache: None,
            });

            let backdrop = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                    targets: &[target(format, Some(wgpu::BlendState::REPLACE))],
                }),
                multiview_mask: None,
                cache: None,
            });

            Pipelines {
                model_culled,
                model_two_sided,
                model_colored_culled,
                model_colored_two_sided,
                point,
                backdrop,
            }
        };
        let pipelines = build(samples);
        // Only a second set when the adapter multisamples at all; otherwise
        // the settled pipelines are already single-sampled.
        let interactive_pipelines = (samples > 1).then(|| build(1));

        let limits = device.limits();
        // Reported once, where a user can see it: an odd picture is usually a
        // device that is not what it looks like (a software rasteriser, no
        // MSAA, no 64-bit atomics for the packed-buffer paths).
        let caps = DeviceCaps {
            int64_atomics: adapter_features.contains(wgpu::Features::SHADER_INT64_ATOMIC_MIN_MAX),
            max_storage_buffer: limits.max_storage_buffer_binding_size,
            max_buffer: limits.max_buffer_size,
            samples,
            format,
            device_type: info.device_type,
        };

        eprintln!("trove: 3D backend {} — {}", adapter_name, caps.summary());

        Ok(Self {
            device,
            queue,
            pipelines,
            interactive_pipelines,
            format,
            samples,
            uniforms,
            bind_group,
            bind_group_layout: layout,
            texture_sampler: sampler,
            env_view,
            edl_bind_group_layout,
            edl_pipeline,
            _shadow_depth: shadow_depth,
            shadow_view,
            shadow_cmp,
            shadow_pipeline,
            _shadow_layout: shadow_layout,
            _shadow_pipeline_layout: shadow_pipeline_layout,
            shadow_bind_group,
            _brdf_texture: brdf_texture,
            brdf_view,
            lut_sampler,
            targets: std::sync::Mutex::new(None),
            adapter: adapter_name,
        })
    }

    /// The targets for this frame's size and sample count, reusing the last
    /// set when neither changed.
    ///
    /// An interactive drag flips the sample count twice per gesture — once
    /// when it starts, once when it ends — so the rebuild cost is paid twice,
    /// not per frame.
    fn targets(
        &self,
        width: u32,
        height: u32,
        samples: u32,
    ) -> std::sync::MutexGuard<'_, Option<Targets>> {
        let mut targets = self
            .targets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = targets.as_ref();
        if current.map(|t| (t.width, t.height, t.samples)) != Some((width, height, samples)) {
            *targets = Some(self.create_targets(width, height, samples));
        }
        targets
    }

    /// Pipelines for a frame: the single-sampled set while the pointer is
    /// down, the multisampled set otherwise.
    fn pipelines(&self, interactive: bool) -> &Pipelines {
        match (interactive, &self.interactive_pipelines) {
            (true, Some(pipelines)) => pipelines,
            _ => &self.pipelines,
        }
    }

    /// Sample count the frame will be drawn with.
    fn sample_count(&self, interactive: bool) -> u32 {
        if interactive && self.interactive_pipelines.is_some() {
            1
        } else {
            self.samples
        }
    }

    fn create_targets(&self, width: u32, height: u32, samples: u32) -> Targets {
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
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                // The post pass reads the resolved image it produced.
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        // With MSAA the pass draws into a multisampled target that resolves
        // into `color`; without it, straight into `color`.
        let msaa = (samples > 1).then(|| {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("trove-3d-msaa"),
                size: extent,
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            (texture, view)
        });
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("trove-3d-depth"),
            size: extent,
            mip_level_count: 1,
            sample_count: samples,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            // Sampled by the post pass, at sample 0.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        // The post pass needs a multisampled depth texture to read; a
        // single-sampled frame has none, so it is drawn without the effect.
        let edl_color = (samples > 1).then(|| {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("trove-3d-edl-color"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            (texture, view)
        });
        let edl_bind_group = edl_color.as_ref().map(|_| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("trove-3d-edl"),
                layout: &self.edl_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&depth_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&color_view),
                    },
                ],
            })
        });
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("trove-3d-readback"),
            size: gpu::staging_len(width, height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Targets {
            width,
            height,
            samples,
            color,
            color_view,
            msaa,
            _depth: depth,
            depth_view,
            edl_color,
            edl_bind_group,
            staging,
        }
    }

    /// Bytes of GPU buffer a mesh will occupy: interleaved vertices plus
    /// the index list.
    ///
    /// Reported rather than enforced. This used to gate the upload — a mesh
    /// over 256 MiB was refused and the whole viewport fell back to the CPU
    /// rasterizer — which turned a model the GPU could in fact hold into a
    /// software render for a reason the user could not see. The number is
    /// still worth having: it is logged before a large upload, so a machine
    /// that does run out of video memory has a line saying which model did it
    /// and what it was expected to cost.
    ///
    /// The three layouts have to be counted as [`render3d::vertex_data`]
    /// and [`render3d::point_data`] actually build them. A flat-shaded mesh
    /// is expanded per face, so its vertex buffer is `triangles × 3` —
    /// counting the source vertices instead under-reports a soup with more
    /// triangles than vertices by a factor of three or more.
    pub fn estimate_gpu_bytes(mesh: &Mesh) -> usize {
        if mesh.is_point_cloud() {
            return mesh.vertex_count() * render3d::PointData::STRIDE as usize;
        }
        if mesh.has_vertex_normals() {
            mesh.vertex_count() * render3d::VertexData::STRIDE as usize
                + mesh.triangle_count() * 3 * 4
        } else {
            mesh.triangle_count() * 3 * render3d::VertexData::STRIDE as usize
        }
    }

    /// [`GpuRenderer::upload`] with the driver's own out-of-memory answer
    /// turned into an `Err` instead of a panic.
    ///
    /// An upload can ask a laptop GPU for more than it has. Without a scope
    /// around it that failure reaches wgpu's fallback handler and aborts the
    /// driver's worker thread — which is the whole process going down, not the
    /// viewport falling back. The scope captures it so the caller can drop to
    /// the CPU rasteriser with a nameable reason.
    pub async fn upload_checked(&self, mesh: &Mesh) -> Result<GpuMesh, GpuUnavailable> {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let uploaded = self.upload(mesh);
        match scope.pop().await {
            Some(error) => Err(GpuUnavailable::OutOfMemory(error.to_string())),
            None => Ok(uploaded),
        }
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
                // A sprite is always facing the camera, whatever its winding.
                cull_backfaces: false,
                meshlets: Vec::new(),
                // Points already carry their colour inside the instance
                // buffer; there is no separate colour pass to make.
                colors: None,
                tex: None,
                bind_group: self.bind_group.clone(),
            };
        }

        // Which way the surface faces decides whether its back faces can be
        // culled. An inside-out file is re-wound on the way into the buffer,
        // so one culled pipeline serves any closed mesh.
        let winding = mesh.winding();
        let flip_winding = winding == Winding::ClosedInward;
        let mut data = render3d::vertex_data_with(mesh, flip_winding);
        // Cut a large mesh into cullable clusters. The triangles are reordered
        // so each cluster is a contiguous run of the index buffer, which is
        // the point of the exercise: one draw per cluster the camera can see,
        // instead of every triangle in the model.
        let meshlets = match meshlet::partition(mesh, meshlet::DEFAULT_MESHLET_TRIANGLES) {
            Some(set) => {
                if let Some(indices) = data.indices.as_mut() {
                    let mut reordered = Vec::with_capacity(indices.len());
                    for &triangle in &set.order {
                        let at = triangle as usize * 3;
                        reordered.extend_from_slice(&indices[at..at + 3]);
                    }
                    *indices = reordered;
                }
                set.meshlets
            }
            None => Vec::new(),
        };
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

        // The file's own colours go in a buffer of their own: only a mesh that
        // has them pays for one, and the coloured pipelines read it as a second
        // vertex buffer beside the interleaved positions and normals.
        let colors = (!data.colors.is_empty()).then(|| {
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("trove-3d-colors"),
                size: (data.color_bytes().len() as u64).max(4),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&buffer, 0, &data.color_bytes());
            buffer
        });

        // The textures become one array, every layer resized to the set's
        // largest edge (capped) because an array demands uniform dimensions;
        // layer 0 is white, which is what the vertices of an untextured
        // primitive point at — "multiply by white" leaves their material
        // colour alone. The coordinates ride a third vertex buffer, present
        // whenever the colours are, because the coloured pipelines declare it.
        let texture_data = mesh
            .texture
            .as_deref()
            .filter(|t| !t.maps.is_empty() && t.maps.len() < NO_TEXTURE as usize);
        // The coordinates and PBR factors come out of the vertex data — which
        // followed the same per-face expansion the positions did — as a third
        // vertex buffer, present whenever the colours are, because the
        // coloured pipelines declare it.
        let tex = colors.as_ref().map(|_| {
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("trove-3d-tex"),
                size: (data.tex.len() as u64 * 4).max(4),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&buffer, 0, &data.tex_bytes());
            buffer
        });

        // The array is budgeted before it is created: a scene with hundreds of
        // maps would otherwise ask for gigabytes and be answered with an OOM.
        // `texture_layout` halves the shared edge until the set fits, and gives
        // up (`None`) when even the smallest useful edge overflows — the mesh
        // then binds the renderer's white stand-in, so it still opens.
        let texture_plan =
            texture_data.and_then(|t| texture_layout(&t.maps).map(|(dim, mips)| (t, dim, mips)));
        // The mesh's own bind group: the shared uniforms plus its texture
        // array — or the renderer's white stand-in when there is nothing to
        // sample, which every pipeline accepts.
        let bind_group = match texture_plan {
            Some((t, dim, mips)) => {
                // Every layer gets the full mip chain: the sampler reads it
                // trilinearly (with 16-tap anisotropy), which is what keeps a
                // minified texture from shimmering the way Blender's viewport
                // — whose images all carry mipmaps — does not.
                let layers = t.maps.len() + 1;
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("trove-3d-textures"),
                    size: wgpu::Extent3d {
                        width: dim,
                        height: dim,
                        depth_or_array_layers: layers as u32,
                    },
                    mip_level_count: mips,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                // Layer 0 is white at every level, and each map's chain is
                // filtered on the host in scene-linear space — the same
                // average a GPU blit between sRGB views computes.
                let chains =
                    std::iter::once(mip_chain(&vec![255u8; (dim * dim * 4) as usize], dim)).chain(
                        t.maps.iter().map(|map| {
                            let base = resize_rgba(&map.rgba, map.width, map.height, dim, dim);
                            mip_chain(&base, dim)
                        }),
                    );
                for (layer, chain) in chains.enumerate() {
                    debug_assert_eq!(chain.len(), mips as usize, "chain covers the chain");
                    for (level, level_bytes) in chain.into_iter().enumerate() {
                        let level_size = (dim >> level).max(1);
                        self.queue.write_texture(
                            wgpu::TexelCopyTextureInfo {
                                texture: &texture,
                                mip_level: level as u32,
                                origin: wgpu::Origin3d {
                                    z: layer as u32,
                                    ..Default::default()
                                },
                                aspect: wgpu::TextureAspect::All,
                            },
                            &level_bytes,
                            wgpu::TexelCopyBufferLayout {
                                offset: 0,
                                bytes_per_row: Some(level_size * 4),
                                rows_per_image: Some(level_size),
                            },
                            wgpu::Extent3d {
                                width: level_size,
                                height: level_size,
                                depth_or_array_layers: 1,
                            },
                        );
                    }
                }
                let view = texture.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2Array),
                    ..Default::default()
                });
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("trove-3d-mesh-textures"),
                    layout: &self.bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: self.uniforms.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&self.texture_sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(&self.env_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::TextureView(&self.shadow_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: wgpu::BindingResource::Sampler(&self.shadow_cmp),
                        },
                        wgpu::BindGroupEntry {
                            binding: 6,
                            resource: wgpu::BindingResource::TextureView(&self.brdf_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 7,
                            resource: wgpu::BindingResource::Sampler(&self.lut_sampler),
                        },
                    ],
                })
            }
            None => self.bind_group.clone(),
        };

        GpuMesh {
            vertices,
            indices,
            vertex_count: data.vertex_count,
            index_count: data.indices.as_ref().map_or(0, |i| i.len() as u32),
            point_count: 0,
            // One double-sided material anywhere in the file takes the whole
            // mesh off the culled pipeline — culling is a pipeline choice, not
            // a per-material one — and the fragment stage culls the single-
            // sided materials' back faces from the same flag the CPU reads.
            cull_backfaces: winding != Winding::TwoSided && !mesh.has_double_sided_material(),
            meshlets,
            colors,
            tex,
            bind_group,
        }
    }

    /// Draw one frame and read it back as tightly packed BGRA, `width *
    /// height * 4` bytes — the same layout the CPU rasterizer produces, so the
    /// UI cannot tell the two apart. `None` when the read-back fails.
    ///
    /// `interactive` frames are drawn without multisampling: they are the ones
    /// the user is dragging, they are about to be scaled up, and the samples
    /// they would spend are on detail nobody can see while the model is
    /// moving.
    ///
    /// `enhance_points` asks for the eye-dome lighting and gap fill a point
    /// cloud wants. It is honoured only on a settled frame that multisampled,
    /// which is exactly when the effect is visible; `false` leaves the frame
    /// as the rasterizer drew it, matching the CPU path's own opt-out.
    ///
    /// `look` is the packed [`HeightLook`]: the mode, the axis, the range and
    /// the colour scale, all of which the vertex stage resolves per vertex.
    ///
    /// [`HeightLook`]: trove_core::media::height_color::HeightLook
    pub fn render(
        &self,
        mesh: &GpuMesh,
        framing: &Framing,
        size: (u32, u32),
        interactive: bool,
        options: &render3d::RenderOptions,
        look: HeightUniforms,
    ) -> Option<Vec<u8>> {
        let (width, height) = (size.0.max(1), size.1.max(1));
        self.queue.write_buffer(
            &self.uniforms,
            0,
            &Uniforms::new(framing, (width, height))
                .with_height(look)
                .to_bytes(),
        );
        let extent = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let samples = self.sample_count(interactive);
        let pipelines = self.pipelines(interactive);
        let targets = self.targets(width, height, samples);
        let Targets {
            color,
            color_view,
            msaa,
            depth_view,
            edl_color,
            edl_bind_group,
            staging,
            ..
        } = targets.as_ref()?;
        let msaa_view = msaa.as_ref().map(|(_, view)| view);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("trove-3d"),
            });
        // The key light's shadow map, baked before anything shades: every
        // model fragment is about to test itself against it. A point cloud
        // never shadows (its eye-dome lighting is what gives it form), and
        // with the switch off the pass runs empty — clearing only, since a
        // cleared map is "nothing in front of you" and every fragment tests
        // out of it fully lit. That is also what keeps the sampling and the
        // skip from disagreeing: the shader never needs a flag.
        if mesh.point_count == 0 {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("trove-3d-shadow"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.shadow_view,
                    depth_ops: Some(wgpu::Operations {
                        // Reversed-Z: the clear is the far end of the range.
                        load: wgpu::LoadOp::Clear(0.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if options.shadows {
                pass.set_pipeline(&self.shadow_pipeline);
                pass.set_bind_group(0, &self.shadow_bind_group, &[]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                match &mesh.indices {
                    Some(indices) => {
                        // The whole buffer, in one call: the light's window
                        // was sized to hold the model, so there is nothing to
                        // cull against — the main camera's meshlets are not
                        // the light's business.
                        pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
                        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                    }
                    None => pass.draw(0..mesh.vertex_count, 0..1),
                }
            }
            drop(pass);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("trove-3d"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: msaa_view.unwrap_or(color_view),
                    depth_slice: None,
                    // `Some` resolves the multisampled target into `color`.
                    resolve_target: msaa_view.map(|_| color_view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth_view,
                    depth_ops: Some(wgpu::Operations {
                        // Reversed-Z clears to zero: "nothing drawn yet" is
                        // the far end of the reversed range.
                        load: wgpu::LoadOp::Clear(0.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // Backdrop first: it covers the viewport and leaves depth alone.
            pass.set_pipeline(&pipelines.backdrop);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);

            if mesh.point_count > 0 {
                // Six vertices per instance: the sprite's two triangles.
                pass.set_pipeline(&pipelines.point);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.draw(0..6, 0..mesh.point_count);
            } else {
                // A mesh with its own colours is drawn by the pipelines that
                // read the colour buffer — while the material switch is on;
                // off, the flat material from the uniform block stands in.
                let colored = mesh.colors.is_some() && options.material_colors;
                pass.set_pipeline(match (colored, mesh.cull_backfaces) {
                    (true, true) => &pipelines.model_colored_culled,
                    (true, false) => &pipelines.model_colored_two_sided,
                    (false, true) => &pipelines.model_culled,
                    (false, false) => &pipelines.model_two_sided,
                });
                // The mesh's own texture array rides its bind group; the
                // backdrop and the points above kept the renderer's default.
                pass.set_bind_group(0, &mesh.bind_group, &[]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                if let Some(colors) = &mesh.colors {
                    pass.set_vertex_buffer(1, colors.slice(..));
                }
                if let Some(tex) = &mesh.tex {
                    pass.set_vertex_buffer(2, tex.slice(..));
                }
                match &mesh.indices {
                    Some(indices) => {
                        pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
                        // A mesh large enough to be partitioned is drawn one
                        // cluster at a time, skipping the ones the frustum
                        // cannot see; everything else stays a single call.
                        match mesh.visible_meshlets(framing) {
                            None => pass.draw_indexed(0..mesh.index_count, 0, 0..1),
                            Some(ranges) => {
                                for range in ranges {
                                    pass.draw_indexed(range, 0, 0..1);
                                }
                            }
                        }
                    }
                    None => {
                        pass.draw(0..mesh.vertex_count, 0..1);
                    }
                }
            }
        }

        // Eye-dome lighting and gap filling, over the depth just drawn. Only a
        // settled point-cloud frame has both the multisampled depth to read
        // and the pixels to spare; the rest read back what the rasterizer
        // wrote directly.
        let mut enhanced = None;
        if options.enhance_points
            && mesh.point_count > 0
            && let (Some((texture, view)), Some(bind_group)) =
                (edl_color.as_ref(), edl_bind_group.as_ref())
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("trove-3d-edl"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.edl_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_bind_group(1, bind_group, &[]);
            pass.draw(0..3, 0..1);
            drop(pass);
            enhanced = Some(texture);
        }
        let source = enhanced.unwrap_or(color);

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: source,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: staging,
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
        let frame = gpu::unpack_bgra(&mapped, width, height, pixel_order(self.format));
        drop(mapped);
        staging.unmap();
        // The targets stay allocated for the next frame; this only lets the
        // device retire the work that just finished.
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
    use trove_core::media::height_color::{
        Field, FieldData, HeightLook, HeightMode, Scale, scale_by_id,
    };

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

    /// The upload budget is only a real guard if the estimate matches the
    /// buffers `upload` actually creates. A flat-shaded mesh is the case that
    /// used to slip through: its vertex buffer is expanded per face, so
    /// counting source vertices under-reported it several times over.
    #[test]
    fn the_size_estimate_matches_the_buffers_upload_builds() {
        let smooth = {
            let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nvn 0 0 1\nf 1//1 2//1 3//1\n";
            trove_core::media::formats::load_obj(obj).expect("mesh parses")
        };
        let flat = {
            // Two triangles over four vertices: the expanded buffer is six
            // corners, so counting source vertices under-reports it by half —
            // the direction of the error that let oversized meshes through.
            let obj = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3\nf 1 3 4\n";
            trove_core::media::formats::load_obj(obj).expect("mesh parses")
        };
        let cloud = {
            let ply = "ply\nformat ascii 1.0\nelement vertex 2\n\
                       property float x\nproperty float y\nproperty float z\n\
                       end_header\n0 0 0\n1 1 1\n";
            trove_core::media::formats::load_ply(ply.as_bytes()).expect("cloud parses")
        };

        for mesh in [&smooth, &flat, &cloud] {
            let data = render3d::vertex_data(mesh);
            let actual: usize = if mesh.is_point_cloud() {
                render3d::point_data(mesh).bytes().len()
            } else {
                data.vertex_bytes().len() + data.index_bytes().map_or(0, |b| b.len())
            };
            assert_eq!(
                GpuRenderer::estimate_gpu_bytes(mesh),
                actual,
                "estimate for {} vertices / {} triangles",
                mesh.vertex_count(),
                mesh.triangle_count()
            );
        }
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
                ("fs_edl", naga::ShaderStage::Fragment),
                ("fs_model", naga::ShaderStage::Fragment),
                ("fs_model_textured", naga::ShaderStage::Fragment),
                ("fs_point", naga::ShaderStage::Fragment),
                ("vs_backdrop", naga::ShaderStage::Vertex),
                ("vs_model", naga::ShaderStage::Vertex),
                ("vs_model_colored", naga::ShaderStage::Vertex),
                ("vs_point", naga::ShaderStage::Vertex),
                ("vs_shadow", naga::ShaderStage::Vertex),
            ]
        );
    }

    /// `vs_model` reads position and normal; `vs_point` adds the point's own
    /// colour and the two scalar channels. One input per attribute its
    /// pipeline's vertex buffer layout describes, in the same order, with the
    /// same widths — and a float apiece for the channels rather than a `vec2`,
    /// which is what the tenth float's byte offset forces.
    #[test]
    fn the_vertex_inputs_match_the_buffer_layouts() {
        let module = module();
        // (location, kind, components) per input, in the order the shader takes
        // them, and the floats one record holds.
        let float = |location: u32, components: u8| (location, naga::ScalarKind::Float, components);
        for (entry_point, expected, floats) in [
            (
                "vs_model",
                vec![float(0, 3), float(1, 3)],
                render3d::VertexData::STRIDE / 4,
            ),
            // The coloured entry reads the interleaved buffer, the colour
            // buffer and the texture/material record: six floats plus
            // eighteen — uv and layers, PBR factors and map layers, the
            // normal/occlusion scalars, the emissive layer and factor, and
            // the alpha test's cutoff and factor with the doubleSided flag.
            (
                "vs_model_colored",
                vec![
                    float(0, 3),
                    float(1, 3),
                    float(2, 3),
                    float(3, 4),
                    float(4, 4),
                    float(5, 2),
                    float(6, 4),
                    float(7, 4),
                ],
                render3d::VertexData::STRIDE / 4
                    + render3d::VertexData::COLOR_STRIDE / 4
                    + render3d::VertexData::TEX_STRIDE / 4,
            ),
            (
                "vs_point",
                vec![
                    float(0, 3),
                    float(1, 3),
                    float(2, 3),
                    float(3, 1),
                    float(4, 1),
                ],
                render3d::PointData::STRIDE / 4,
            ),
        ] {
            let entry = module
                .entry_points
                .iter()
                .find(|entry| entry.name == entry_point)
                .unwrap_or_else(|| panic!("{entry_point} present"));

            let mut inputs: Vec<(u32, naga::ScalarKind, u8)> = Vec::new();
            for argument in &entry.function.arguments {
                let Some(naga::Binding::Location { location, .. }) = argument.binding else {
                    continue;
                };
                // A scalar attribute is a whole float of its own, and a vector
                // one per component: the same widths the vertex buffer's format
                // names.
                let (kind, components) = match module.types[argument.ty].inner {
                    naga::TypeInner::Scalar(scalar) => (scalar.kind, 1u8),
                    naga::TypeInner::Vector { size, scalar } => (scalar.kind, size as u8),
                    _ => panic!("{entry_point} input @location({location}) is not a number"),
                };
                inputs.push((location, kind, components));
            }
            inputs.sort_by_key(|(location, _, _)| *location);
            assert_eq!(inputs, expected, "{entry_point} inputs");
            // Every component of every input, times four bytes: the declared stride.
            let width: u64 = inputs
                .iter()
                .map(|(_, _, components)| *components as u64)
                .sum();
            assert_eq!(width, floats, "{entry_point} stride");
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
        let expected: [(&str, u32); 15] = [
            ("view_proj", 64),
            ("view_proj_shadow", 64),
            ("material", 16),
            ("params", 16),
            ("lights", 4 * 3 * 16),
            ("basis", 3 * 16),
            ("env_sh", 4 * 16),
            ("params2", 16),
            ("eye", 16),
            ("viewport", 16),
            ("bg_top", 16),
            ("bg_bottom", 16),
            ("coloring", 16),
            ("coloring_params", 16),
            (
                "ramp",
                trove_core::media::height_color::RAMP_STOPS as u32 * 16,
            ),
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

    /// The shader only proves the post pass parses. This proves the device
    /// accepts the pipelines — including the multisampled depth binding the
    /// main pipelines never mention — and that the pass actually changes a
    /// point-cloud frame. Skipped where there is no usable adapter, which is
    /// the same condition the viewport falls back to the CPU on.
    #[test]
    fn the_eye_dome_pass_runs_on_a_real_device() {
        let Ok(renderer) = GpuRenderer::new() else {
            eprintln!("no graphics device: skipping the GPU frame test");
            return;
        };
        // Points on a sphere: a settled frame of it has depth steps between
        // neighbouring pixels, which is what the lighting darkens.
        let mut positions = Vec::new();
        for index in 0..4_000 {
            let t = index as f32 / 4_000.0;
            let (sin, cos) = (t * std::f32::consts::TAU * 8.0).sin_cos();
            let y = 1.0 - 2.0 * t;
            let r = (1.0 - y * y).max(0.0).sqrt();
            positions.push([r * cos, y, r * sin]);
        }
        let mesh = Mesh::from_parts(positions, Vec::new(), Vec::new(), Vec::new()).expect("cloud");
        let uploaded = renderer.upload(&mesh);
        let framing = render3d::Camera::default().framing(mesh.bounds, 1.0);
        let size = (160, 120);

        let plain = renderer
            .render(
                &uploaded,
                &framing,
                size,
                false,
                &render3d::RenderOptions::default(),
                HeightUniforms::default(),
            )
            .expect("a plain frame comes back");
        let enhanced = renderer
            .render(
                &uploaded,
                &framing,
                size,
                false,
                &render3d::RenderOptions {
                    enhance_points: true,
                    ..Default::default()
                },
                HeightUniforms::default(),
            )
            .expect("an enhanced frame comes back");
        assert_eq!(plain.len(), enhanced.len());
        assert_ne!(
            plain, enhanced,
            "the post pass has to change a point-cloud frame"
        );
        // Its output is a picture, not a cleared target.
        let lit = enhanced.as_chunks::<4>().0;
        assert!(
            lit.iter()
                .any(|pixel| pixel[0] > 0 || pixel[1] > 0 || pixel[2] > 0),
            "the enhanced frame is all black"
        );
    }

    /// A triangulated plane with normals: the indexed layout a large mesh has
    /// to be in for the partitioner to take it.
    fn grid_mesh(n: usize) -> Mesh {
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        for z in 0..=n {
            for x in 0..=n {
                positions.push([x as f32, 0.0, z as f32]);
                normals.push([0.0, 1.0, 0.0]);
            }
        }
        let mut triangles = Vec::new();
        let row = (n + 1) as u32;
        for z in 0..n as u32 {
            for x in 0..n as u32 {
                let corner = z * row + x;
                triangles.push([corner, corner + 1, corner + row]);
                triangles.push([corner + 1, corner + row + 1, corner + row]);
            }
        }
        Mesh::from_parts(positions, normals, Vec::new(), triangles).expect("grid builds")
    }

    /// Render one real model file through the GPU and dump the frame, to see
    /// exactly what the viewport sees. Runs only when `TROVE_DEBUG_GLB` names
    /// a file; the frame lands at `/tmp/trove-gpu-render.png`.
    #[test]
    fn a_real_file_renders_on_the_gpu() {
        let Ok(path) = std::env::var("TROVE_DEBUG_GLB") else {
            return;
        };
        let Ok(renderer) = GpuRenderer::new() else {
            eprintln!("no graphics device: skipping the real-file GPU test");
            return;
        };
        let mesh = trove_core::media::formats::load(std::path::Path::new(&path))
            .expect("the debug file parses");
        eprintln!(
            "mesh: {} vertices, {} triangles, winding={:?}, colors={}",
            mesh.positions.len(),
            mesh.triangle_count(),
            mesh.winding(),
            mesh.colors.len()
        );
        let uploaded = renderer.upload(&mesh);
        eprintln!(
            "gpu mesh: meshlets={} colors={} cull={}",
            uploaded.meshlets.len(),
            uploaded.colors.is_some(),
            uploaded.cull_backfaces
        );

        for (yaw, pitch, zoom) in [
            (0.62f32, 0.34f32, 1.0),
            (0.0f32, 0.08f32, 0.35),
            (std::f32::consts::PI, 0.08, 0.35),
            (std::f32::consts::PI, 0.08, 0.2),
            (0.0, 0.5, 0.3),
            (0.35, 0.1, 0.18),
        ] {
            let camera = render3d::Camera {
                yaw,
                pitch,
                zoom,
                ..render3d::Camera::default()
            };
            let framing = camera.framing(mesh.bounds, 1.0);
            let culled = uploaded.visible_meshlets(&framing);
            eprintln!(
                "zoom {zoom}: meshlets drawn = {}",
                match &culled {
                    None => uploaded.meshlets.len(),
                    Some(ranges) => ranges.len(),
                }
            );
            let frame = renderer
                .render(
                    &uploaded,
                    &framing,
                    (800, 600),
                    false,
                    &render3d::RenderOptions::default(),
                    HeightUniforms::default(),
                )
                .expect("a frame comes back");
            dump_png(
                &format!("/tmp/trove-gpu-view-{yaw}-{pitch}-{zoom}.png"),
                &frame,
            );
        }
    }

    /// Write a BGRA frame out as an 8-bit RGB PNG (stored deflate, no deps).
    fn dump_png(path: &str, frame: &[u8]) {
        use std::io::Write;
        let (width, height) = (800u32, 600u32);
        let mut raw = Vec::with_capacity(frame.len());
        for y in 0..height {
            raw.push(0); // filter: none
            for x in 0..width {
                let i = ((y * width + x) as usize) * 4;
                raw.extend_from_slice(&[frame[i + 2], frame[i + 1], frame[i]]);
            }
        }
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        push_png_chunk(&mut png, b"IHDR", &ihdr);
        push_png_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
        push_png_chunk(&mut png, b"IEND", &[]);
        std::fs::File::create(path)
            .unwrap()
            .write_all(&png)
            .unwrap();
        eprintln!("wrote {path}");
    }

    fn push_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = 0xFFFF_FFFFu32;
        for byte in kind.iter().chain(data) {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        out.extend_from_slice(&(crc ^ 0xFFFF_FFFF).to_be_bytes());
    }

    fn zlib_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut chunks = data.chunks(65535).peekable();
        if data.is_empty() {
            out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
        }
        while let Some(chunk) = chunks.next() {
            out.push(if chunks.peek().is_none() { 1 } else { 0 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    /// A mesh large enough to be partitioned is cut into clusters, culled
    /// against the real frustum, and drawn without the device complaining —
    /// the index reorder and the per-cluster draws together.
    #[test]
    fn a_large_mesh_is_partitioned_and_culled_on_a_real_device() {
        let Ok(renderer) = GpuRenderer::new() else {
            eprintln!("no graphics device: skipping the meshlet frame test");
            return;
        };
        let mesh = grid_mesh(256); // 131 072 triangles
        assert!(mesh.triangle_count() >= meshlet::MIN_MESHLET_TRIANGLES);
        let uploaded = renderer.upload(&mesh);
        assert!(
            uploaded.meshlets.len() > 1,
            "a large mesh has to be partitioned"
        );

        // Framed from far away every cluster is visible: one draw, no culling.
        let wide = render3d::Camera {
            zoom: render3d::MAX_ZOOM,
            ..render3d::Camera::default()
        }
        .framing(mesh.bounds, 1.0);
        assert!(uploaded.visible_meshlets(&wide).is_none());

        // Close up, part of the model is off screen and its clusters drop out.
        let close = render3d::Camera {
            zoom: render3d::MIN_ZOOM,
            ..render3d::Camera::default()
        }
        .framing(mesh.bounds, 1.0);
        let visible = uploaded
            .visible_meshlets(&close)
            .expect("the close view culls");
        assert!(!visible.is_empty() && visible.len() < uploaded.meshlets.len());

        let frame = renderer
            .render(
                &uploaded,
                &close,
                (160, 120),
                false,
                &render3d::RenderOptions::default(),
                HeightUniforms::default(),
            )
            .expect("a frame comes back");
        assert!(
            frame
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[0] > 0 || pixel[1] > 0 || pixel[2] > 0),
            "the culled mesh drew nothing"
        );
    }

    /// The classification has to arrive through the instance buffer and come back
    /// out as the palette's colour, or the frame and the thumbnail disagree about
    /// a cloud's classes — and neither looks wrong on its own.
    #[test]
    fn the_gpu_paints_a_cloud_from_its_classification() {
        let Ok(renderer) = GpuRenderer::new() else {
            eprintln!("no graphics device: skipping the classified frame test");
            return;
        };
        let cloud = |classes: [u8; 2]| {
            let ply = format!(
                "ply\nformat ascii 1.0\nelement vertex 2\n\
                 property float x\nproperty float y\nproperty float z\n\
                 property uchar classification\nend_header\n\
                 -0.4 0 0 {}\n0.4 0 0 {}\n",
                classes[0], classes[1]
            );
            trove_core::media::formats::load_ply(ply.as_bytes()).expect("a classified cloud parses")
        };
        // The same look for both clouds: the ASPRS palette, whose domain is its
        // own 23 classes rather than whatever a cloud happens to carry.
        let look = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Class,
            scale: Scale::Preset(scale_by_id("asprs")),
            ..Default::default()
        };
        let frame = |classes: [u8; 2]| {
            let mesh = cloud(classes);
            let bounds = mesh.bounds;
            let uploaded = renderer.upload(&mesh);
            renderer
                .render(
                    &uploaded,
                    &render3d::Camera::default().framing(bounds, 1.0),
                    (160, 120),
                    false,
                    &render3d::RenderOptions::default(),
                    look.resolve(&FieldData::geometry(&bounds)).uniforms(),
                )
                .expect("a frame comes back")
        };
        let warm = |frame: &[u8]| {
            frame
                .as_chunks::<4>()
                .0
                .iter()
                .map(|pixel| pixel[2] as i32 - pixel[0] as i32)
                .max()
                .unwrap_or(0)
        };
        // Class 6 is a building yellow and class 2 a ground brown. The material
        // both paths fall back to — and the backdrop — are blue-grey, so a warm
        // pixel can only have come out of the classification. The margin is
        // modest — the studio lights are near-neutral, so they tint every class
        // toward grey — but the unclassified frame stays on the cool side, and
        // the two never meet.
        let buildings = frame([2, 6]);
        assert!(
            warm(&buildings) > 10,
            "the palette's warm end never appeared"
        );
        // Class 0 is "not classified", which the palette paints white: the
        // same two points, the same look. The near-neutral studio lights tint
        // every class toward grey, so warmth is a matter of degree — the
        // yellow end stays measurably warmer than the white.
        let unclassified = frame([0, 0]);
        assert!(
            warm(&buildings) >= 15,
            "the palette's warm end never appeared"
        );
        assert!(
            warm(&unclassified) <= 10,
            "a white class came out of the palette warm"
        );
        assert_ne!(buildings, unclassified, "the classes were ignored");
    }

    /// A textured mesh's atlas is uploaded with its full mip chain — every
    /// level the sampler's trilinear read expects, at the floor-halved sizes
    /// the descriptor names — and the mesh renders through it. The map here
    /// is deliberately an odd 3×3, the size whose chain is not a clean
    /// power-of-two walk; a level whose extent disagreed with the chain would
    /// fail the upload or draw garbage. Skipped where there is no adapter.
    #[test]
    fn a_textured_mesh_uploads_its_mip_chain_and_renders() {
        let Ok(renderer) = GpuRenderer::new() else {
            eprintln!("no graphics device: skipping the textured frame test");
            return;
        };
        let positions = vec![[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let mut mesh = Mesh::from_parts(
            positions.clone(),
            vec![[0.0f32, 0.0, 1.0]; 3],
            vec![[0.8f32, 0.8, 0.8]; 3],
            vec![[0, 1, 2]],
        )
        .expect("triangle builds");
        mesh.texture = Some(Box::new(trove_core::media::formats::types::TextureData {
            uv: vec![[0.25, 0.25]; 3],
            slot: vec![0; 3],
            // The remaining layers point at the white stand-in; the alpha
            // test is off (negative cutoff) and the PBR factors are the
            // spec defaults the white layer's samples leave standing.
            mr_slot: vec![trove_core::media::formats::types::NO_TEXTURE; 3],
            factors: vec![[0.0, 1.0]; 3],
            normal_slot: vec![trove_core::media::formats::types::NO_TEXTURE; 3],
            normal_scale: vec![1.0; 3],
            ao_slot: vec![trove_core::media::formats::types::NO_TEXTURE; 3],
            ao_strength: vec![1.0; 3],
            emissive_slot: vec![trove_core::media::formats::types::NO_TEXTURE; 3],
            emissive_factor: vec![[0.0; 3]; 3],
            alpha_cutoff: vec![-1.0; 3],
            alpha_factor: vec![1.0; 3],
            double_sided: vec![false; 3],
            // One odd-sized red map: dim 3, chain [3, 1].
            maps: vec![trove_core::media::formats::types::TextureMap {
                rgba: {
                    let mut v = vec![255u8; 3 * 3 * 4];
                    for pixel in v.as_chunks_mut::<4>().0 {
                        pixel[1] = 0;
                        pixel[2] = 0;
                    }
                    v
                },
                width: 3,
                height: 3,
            }],
        }));
        let uploaded = renderer.upload(&mesh);
        let frame = renderer
            .render(
                &uploaded,
                &render3d::Camera::default().framing(mesh.bounds, 1.0),
                (160, 120),
                false,
                &render3d::RenderOptions::default(),
                HeightUniforms::default(),
            )
            .expect("a textured frame comes back");
        assert!(
            frame
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[2] > 0 || pixel[1] > 0 || pixel[0] > 0),
            "the textured mesh drew nothing"
        );
    }

    /// A small occluder floating over a wide ground plane casts a shadow the
    /// renderer can see: the frame with the key light's shadow pass differs
    /// from the one without, and only ever gets darker — the shadow scales
    /// the key light's share down from its full strength, never up. Skipped
    /// where there is no adapter.
    #[test]
    fn an_occluder_darkens_the_ground_on_the_gpu() {
        let Ok(renderer) = GpuRenderer::new() else {
            eprintln!("no graphics device: skipping the shadow frame test");
            return;
        };
        let mesh = occluder_over_ground();
        let uploaded = renderer.upload(&mesh);
        let framing = render3d::Camera::default().framing(mesh.bounds, 1.0);
        let frame = |shadows: bool| {
            renderer
                .render(
                    &uploaded,
                    &framing,
                    (320, 240),
                    false,
                    &render3d::RenderOptions {
                        shadows,
                        ..Default::default()
                    },
                    HeightUniforms::default(),
                )
                .expect("a frame comes back")
        };
        let lit = frame(true);
        let plain = frame(false);
        assert_ne!(lit, plain, "the shadow pass has to change the frame");
        let mean = |frame: &[u8]| {
            frame
                .as_chunks::<4>()
                .0
                .iter()
                .map(|pixel| pixel[0] as u64 + pixel[1] as u64 + pixel[2] as u64)
                .sum::<u64>()
        };
        assert!(
            mean(&lit) < mean(&plain),
            "shadows only darken: {} against {}",
            mean(&lit),
            mean(&plain)
        );
    }

    /// A wide ground plane at y = 0 carrying a small box at its centre: the
    /// shape the shadow test needs — a caster above a receiver, with the key
    /// light above the horizon, so the box's shadow falls on visible ground.
    fn occluder_over_ground() -> Mesh {
        trove_core::media::formats::load_obj(
            "vn 0 1 0\n\
             v -5 0 -5\nv 5 0 -5\nv 5 0 5\nv -5 0 5\n\
             f 1//1 3//1 2//1\nf 1//1 4//1 3//1\n\
             v 0.4 0.4 0.4\nv 0.6 0.4 0.4\nv 0.6 0.4 0.6\nv 0.4 0.4 0.6\n\
             v 0.4 0.8 0.4\nv 0.6 0.8 0.4\nv 0.6 0.8 0.6\nv 0.4 0.8 0.6\n\
             f 5//1 7//1 6//1\nf 5//1 8//1 7//1\n\
             f 9//1 10//1 11//1\nf 9//1 11//1 12//1\n\
             f 5//1 6//1 10//1\nf 5//1 10//1 9//1\n\
             f 6//1 7//1 11//1\nf 6//1 11//1 10//1\n\
             f 7//1 8//1 12//1\nf 7//1 12//1 11//1\n\
             f 8//1 5//1 9//1\nf 8//1 9//1 12//1\n",
        )
        .expect("the occluder scene parses")
    }

    /// The display matrices in the shader must be the CPU's `display_color`
    /// matrices in column-major form — WGSL `mat3x3` arguments are columns,
    /// and copying the CPU's *rows* in as columns silently transposes the
    /// transform. That transpose held neutral greys through the wrong
    /// rotation and painted every GPU-rendered model with a blue-violet
    /// cast, which is why this is pinned number for number.
    #[test]
    fn the_display_matrices_match_the_cpu_row_form() {
        // The CPU's row-multiplication constants, from `display_color`.
        let cpu: [(&str, [[f32; 3]; 3]); 4] = [
            (
                "srgb_to_rec2020",
                [
                    [0.627_403_9, 0.329_283, 0.043_313_1],
                    [0.069_097_3, 0.919_540_4, 0.011_362_3],
                    [0.016_391_4, 0.088_013_3, 0.895_595_3],
                ],
            ),
            (
                "rec2020_to_srgb",
                [
                    [1.660_491, -0.587_641_1, -0.072_849_9],
                    [-0.124_550_5, 1.132_899_9, -0.008_349_4],
                    [-0.018_150_8, -0.100_578_9, 1.118_729_7],
                ],
            ),
            (
                "agx_inset",
                [
                    [0.856_627_2, 0.095_121_2, 0.048_251_6],
                    [0.137_319, 0.761_242, 0.101_439],
                    [0.111_898_2, 0.076_799_4, 0.811_302_4],
                ],
            ),
            (
                "agx_outset",
                [
                    [1.127_100_6, -0.110_606_6, -0.016_493_9],
                    [-0.141_329_8, 1.157_823_7, -0.016_493_9],
                    [-0.141_329_8, -0.110_606_6, 1.251_936_4],
                ],
            ),
        ];
        for (name, rows) in cpu {
            let block = SHADER
                .split_once(&format!("const {name} = mat3x3<f32>("))
                .map(|(_, rest)| rest)
                .unwrap_or_else(|| panic!("the shader declares const {name}"));
            let block = &block[..block.find(");").expect("the matrix block closes")];
            let values: Vec<f32> = block
                .split(&[',', '(', ')'][..])
                .filter_map(|token| token.trim().parse::<f32>().ok())
                .collect();
            assert_eq!(values.len(), 9, "{name} holds nine floats");
            // WGSL column j against CPU column j: the transposed copy put
            // the CPU's row 0 where column 0 belongs, which is exactly the
            // drift this test exists to catch.
            for column in 0..3 {
                for row in 0..3 {
                    let got = values[column * 3 + row];
                    let want = rows[row][column];
                    assert!(
                        (got - want).abs() < 1e-5,
                        "{name} column {column} row {row}: {got} against {want}"
                    );
                }
            }
        }
    }

    /// A plane facing the key light must shade uniformly: nothing stands
    /// between it and the light, so its shadow map reads "nothing nearer"
    /// everywhere. The shadow lookup used to fold the light-space NDC into
    /// the map's uv without flipping v, so the plane sampled its own
    /// mirrored depths and a band of false shadow crossed every lit face.
    /// Skipped where there is no adapter.
    #[test]
    fn a_facing_plane_shades_without_a_false_shadow() {
        let Ok(renderer) = GpuRenderer::new() else {
            eprintln!("no graphics device: skipping the plane shadow test");
            return;
        };
        let mesh = trove_core::media::formats::load_obj(
            "vn 0 0 1\n\
             v -1 -1 0\nv 1 -1 0\nv 1 1 0\nv -1 1 0\n\
             f 1//1 2//1 3//1\nf 1//1 3//1 4//1\n",
        )
        .expect("the plane parses");
        let uploaded = renderer.upload(&mesh);
        let camera = render3d::Camera {
            yaw: 0.0,
            pitch: 0.0,
            ..render3d::Camera::default()
        };
        let frame = renderer
            .render(
                &uploaded,
                &camera.framing(mesh.bounds, 1.0),
                (320, 240),
                false,
                &render3d::RenderOptions::default(),
                HeightUniforms::default(),
            )
            .expect("a frame comes back");
        // The middle half of the frame is plane, backdrop nowhere in sight.
        let pixels = frame.as_chunks::<4>().0;
        let (width, height) = (320usize, 240usize);
        let mut darkest = 255u32;
        for y in height / 4..height * 3 / 4 {
            for x in width / 4..width * 3 / 4 {
                let pixel = &pixels[y * width + x];
                let brightness = (pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32) / 3;
                darkest = darkest.min(brightness);
            }
        }
        assert!(
            darkest >= 60,
            "a facing plane shaded to near-black ({darkest}): the shadow test \
             is answering against the wrong part of the map"
        );
    }

    /// Where a model open spends its time: device bring-up, the upload, the
    /// first GPU frame, and the CPU first frame it would have drawn instead.
    /// Prints rather than asserts — the numbers are the point. Runs when
    /// `TROVE_DEBUG_GPU_TIMING` is set; skipped where there is no adapter.
    #[test]
    fn a_model_open_timed_stage_by_stage() {
        if std::env::var_os("TROVE_DEBUG_GPU_TIMING").is_none() {
            return;
        }
        let Ok(renderer) = GpuRenderer::new() else {
            eprintln!("no graphics device: skipping the timing run");
            return;
        };
        // A grid big enough to matter, carrying a 1024² map like a real file.
        let mesh = {
            let mut mesh = grid_mesh(320); // 204 800 triangles
            let side = 1024usize;
            let mut rgba = vec![0u8; side * side * 4];
            for pixel in rgba.as_chunks_mut::<4>().0 {
                pixel.copy_from_slice(&[200, 150, 100, 255]);
            }
            let count = mesh.positions.len();
            mesh.texture = Some(Box::new(trove_core::media::formats::types::TextureData {
                uv: vec![[0.5, 0.5]; count],
                slot: vec![0; count],
                mr_slot: vec![trove_core::media::formats::types::NO_TEXTURE; count],
                factors: vec![[0.0, 1.0]; count],
                normal_slot: vec![trove_core::media::formats::types::NO_TEXTURE; count],
                normal_scale: vec![1.0; count],
                ao_slot: vec![trove_core::media::formats::types::NO_TEXTURE; count],
                ao_strength: vec![1.0; count],
                emissive_slot: vec![trove_core::media::formats::types::NO_TEXTURE; count],
                emissive_factor: vec![[0.0; 3]; count],
                alpha_cutoff: vec![-1.0; count],
                alpha_factor: vec![1.0; count],
                double_sided: vec![false; count],
                maps: vec![trove_core::media::formats::types::TextureMap {
                    rgba,
                    width: side as u32,
                    height: side as u32,
                }],
            }));
            mesh
        };
        let mark = |what: &str, started: std::time::Instant| {
            eprintln!("{what}: {:.1?}", started.elapsed());
        };

        let started = std::time::Instant::now();
        let _data = render3d::vertex_data_with(&mesh, false);
        mark("vertex data", started);
        let started = std::time::Instant::now();
        let _partitioned = trove_core::media::formats::meshlet::partition(
            &mesh,
            trove_core::media::formats::meshlet::DEFAULT_MESHLET_TRIANGLES,
        );
        mark("meshlet partition", started);
        let started = std::time::Instant::now();
        if let Some(map) = mesh.texture.as_ref().unwrap().maps.first() {
            let base = trove_core::media::formats::types::resize_rgba(
                &map.rgba, map.width, map.height, 1024, 1024,
            );
            let _ = trove_core::media::formats::types::mip_chain(&base, 1024);
        }
        mark("one 1024² map: resize + mip chain", started);

        let started = std::time::Instant::now();
        let uploaded = renderer.upload(&mesh);
        mark("gpu upload (meshlets + atlas + mip chain)", started);

        let started = std::time::Instant::now();
        let frame = renderer
            .render(
                &uploaded,
                &render3d::Camera::default().framing(mesh.bounds, 1.0),
                (800, 600),
                false,
                &render3d::RenderOptions::default(),
                HeightUniforms::default(),
            )
            .expect("a frame comes back");
        mark("first gpu frame (incl. readback)", started);
        eprintln!("frame bytes: {}", frame.len());

        let started = std::time::Instant::now();
        let _ = render3d::render(&mesh, &render3d::Camera::default(), 800, 600, 1, 1.0);
        mark("cpu first frame at full size (debug build!)", started);
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
