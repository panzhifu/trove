//! CPU software rasterizer for mesh previews.
//!
//! Rendering happens on the CPU on purpose: the preview is only a few hundred
//! pixels across, it runs off the UI thread next to the existing thumbnail
//! pipeline, and it keeps the crate free of a graphics stack. The output is a
//! plain BGRA buffer, which the app hands to gpui exactly like a decoded video
//! frame (`ImageSource::Render`).
//!
//! The pipeline is the textbook one, sized for a turntable preview:
//!
//! 1. the mesh is normalised into a unit bounding sphere so any model, whatever
//!    its real units, frames identically;
//! 2. an orbiting camera builds an orthonormal basis and projects with a
//!    perspective divide;
//! 3. triangles are near-plane clipped, then rasterised with an edge-function
//!    test against a `1/z` depth buffer (`1/z` is linear in screen space, so
//!    plain barycentric interpolation of it is exact);
//! 4. the key studio light casts a shadow map: the mesh is rasterised a second
//!    time from the light's orthographic view, and every shaded pixel tests
//!    its own light-space depth against it (3×3 PCF). The three dim fills and
//!    the environment do not cast — a shadowed surface keeps a quarter of its
//!    light, which is what keeps the shadow from going pitch black;
//! 5. shading is Blender's: the Solid viewport's studio lighting — four
//!    camera-anchored lights with soft terminators, a dielectric specular and
//!    a Fresnel lift at grazing angles — flat when the file carried no
//!    normals, Gouraud when it did.
//!
//! The lights are fixed to the camera, as Blender's are with world
//! orientation off: orbiting keeps the same key from the viewer's upper left
//! while the three dim fills keep the form reading from every angle.

use super::formats::point_cloud::Frustum;
use super::formats::types::{Bounds, Mesh, NO_TEXTURE, TextureData, TextureMap};
use super::height_color::{HeightField, Sample};

/// Vertical field of view used to frame a model, in degrees.
pub const FOV_DEG: f32 = 35.0;
/// Weight of the eye-dome lighting term, shared with the GPU post pass so a
/// cloud lit on either renderer looks the same. Low enough that a flat surface
/// stays flat, high enough that a fold in a scan is obvious.
pub const EDL_STRENGTH: f32 = 1.6;
/// Extra room left around the model when it is framed.
const FIT_MARGIN: f32 = 1.12;
/// Pitch stops just short of the poles, where the view basis degenerates.
const MAX_PITCH: f32 = 1.45;
/// Distance multiplier limits; 1.0 frames the whole model.
pub const MIN_ZOOM: f32 = 0.35;
pub const MAX_ZOOM: f32 = 8.0;
/// Resolutions outside this range are refused, so one frame cannot eat memory.
const MAX_EDGE: u32 = 2048;
const MIN_EDGE: u32 = 16;
/// Smallest edge [`auto_size`] ever picks, unless the caller asked for less.
const AUTO_SIZE_FLOOR: u32 = 320;
/// Supersampling beyond 2× costs more than it is worth here.
pub const MAX_SUPERSAMPLE: u32 = 2;

/// Which studio light casts shadows: the key light, the only strong one. The
/// three fills carry three to nine percent of its diffuse each, so shadowing
/// them would cost three more shadow passes to darken a shadow by a shade.
/// The GPU shader reads the same index — [`KEY_LIGHT`] there mirrors this.
pub const KEY_LIGHT: usize = 1;
/// Edge of the shadow map the GPU renderer bakes. The GPU path bakes once per
/// frame, and a 1024 map keeps a self-shadow crisp on a 1600-pixel viewport.
pub const SHADOW_MAP_SIZE: u32 = 1024;
/// Edge of the shadow map the software rasterizer bakes. Thumbnails render
/// once per import and the CPU fallback pays for it every frame, so a quarter
/// of the GPU's map is where the cost lands; at thumbnail sizes the softness
/// difference is a rounding error.
pub const CPU_SHADOW_MAP_SIZE: u32 = 512;
/// Half-extent of the light's orthographic window, in bounding-sphere radii:
/// the model spans at most one radius in any direction, so 1.25 leaves a
/// margin around the projection without wasting map resolution.
const SHADOW_HALF_EXTENT: f32 = 1.25;
/// Depth range of the light's orthographic window, in bounding-sphere radii —
/// ±2 covers the whole sphere whatever direction the light looks from, so the
/// near plane never clips a caster or a receiver.
const SHADOW_RANGE: f32 = 2.0;

/// The key studio light's orthographic framing of the model: the basis it
/// looks down, and the window that maps the model onto its shadow map.
///
/// Built per frame from the camera's framing — the rig is camera-anchored, so
/// the light moves with the orbit — and shared by both renderers: the CPU
/// rasterises its depth map through [`Self::uv_depth`] and the GPU packs
/// [`Self::view_projection`] into the uniform block. Sharing one construction
/// is what keeps a thumbnail's shadow and the viewport's on the same side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowFraming {
    /// The pivot the window is centred on (the camera's own pivot).
    center: [f32; 3],
    /// Orthonormal light basis in model space: `right`/`up` span the shadow
    /// map, `forward` is the direction the light travels (away from the light,
    /// the negated key-light direction).
    right: [f32; 3],
    up: [f32; 3],
    forward: [f32; 3],
    /// Half-extent of the orthographic window, in model units. The uniform
    /// block's shadow-texel scalar is one texel's share of it, so the GPU
    /// side reads this one field.
    pub half_extent: f32,
    /// Depth range of the orthographic window, in model units.
    range: f32,
    /// Reciprocal of the bounding-sphere radius, for the unit-space mapping.
    inv_radius: f32,
}

impl ShadowFraming {
    /// Where a unit-space point lands on the shadow map: `[u, v, depth]`, all
    /// in 0..=1. `depth` is reversed like every depth this renderer writes —
    /// nearer to the light is larger, and "nothing drawn" is zero.
    ///
    /// This is the CPU rasteriser's path; the GPU reads the same mapping out
    /// of [`Self::view_projection`].
    pub fn uv_depth(&self, unit: [f32; 3]) -> [f32; 3] {
        // u = 0.5·dot(p − center, right)/half_extent + 0.5, and the CPU's
        // unit-space position is (p − center)·inv_radius.
        let scale = 0.5 / (self.half_extent * self.inv_radius);
        let depth_scale = 1.0 / (2.0 * self.range * self.inv_radius);
        [
            0.5 + dot(unit, self.right) * scale,
            0.5 + dot(unit, self.up) * scale,
            0.5 - dot(unit, self.forward) * depth_scale,
        ]
    }

    /// Model space → the shadow map's clip space, column-major for a WGSL
    /// `mat4x4<f32>`: `xy` is NDC (the shader folds it to the map's uv with
    /// `clip.xy * 0.5 + 0.5`), `z` the reversed depth (`1` at the near plane,
    /// `0` at the far, wgpu's 0..1 clip range), `w` 1 — an orthographic
    /// projection, so the perspective divide is inert and the depth
    /// interpolation stays affine.
    pub fn view_projection(&self) -> [[f32; 4]; 4] {
        let gain = 1.0 / self.half_extent;
        let depth_gain = -1.0 / (2.0 * self.range);
        let row = |axis: [f32; 3], gain: f32, offset: f32| -> [f32; 4] {
            [
                axis[0] * gain,
                axis[1] * gain,
                axis[2] * gain,
                offset - gain * dot(self.center, axis),
            ]
        };
        let x = row(self.right, gain, 0.0);
        let y = row(self.up, gain, 0.0);
        let z = row(self.forward, depth_gain, 0.5);
        let w = [0.0, 0.0, 0.0, 1.0];
        // Column-major for WGSL, clip order (x, y, z, w) — the same
        // transposition [`Framing::view_projection`] spells out.
        [
            [x[0], y[0], z[0], w[0]],
            [x[1], y[1], z[1], w[1]],
            [x[2], y[2], z[2], w[2]],
            [x[3], y[3], z[3], w[3]],
        ]
    }
}

/// Frame the key studio light's orthographic shadow window around the model.
///
/// The light looks straight down its direction with world up as the roll
/// hint — flipping to the x axis when the light looks straight down itself,
/// where the cross product would degenerate. The window follows the camera's
/// pivot, so a panned model keeps its shadow frame.
pub fn shadow_framing(framing: &Framing) -> ShadowFraming {
    let key = model_space_lights(framing)[KEY_LIGHT];
    let forward = neg(key.direction);
    let hint = if forward[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let right = normalize(cross(hint, forward));
    let up = cross(forward, right);
    let radius = 1.0 / framing.inv_radius;
    ShadowFraming {
        center: framing.center,
        right,
        up,
        forward,
        half_extent: SHADOW_HALF_EXTENT * radius,
        range: SHADOW_RANGE * radius,
        inv_radius: framing.inv_radius,
    }
}

/// The studio rig Blender's Solid viewport defaults to — `BKE_studiolight_default`
/// in Blender's `studiolight.c`, the internal "Default" preset, copied
/// verbatim: one key light from the viewer's upper left carrying almost all
/// the diffuse, and three dim fills (a rim behind, a fill above, a kick below)
/// whose job is specular and keeping the form legible from any orbit angle.
/// The directions are view space — x right, y up, z into the screen — with the
/// key light's z sign turned from Blender's toward-viewer convention to ours.
pub const STUDIO_LIGHTS: [StudioLight; 4] = [
    // The rim: behind the object, half-wrapped so edges keep a sliver of light.
    StudioLight {
        direction: [-0.352546, 0.170931, 0.920051],
        wrap: 0.526620,
        diffuse: [0.033103; 3],
        specular: [0.266761; 3],
    },
    // The key: the only strong light, from the viewer's upper left.
    StudioLight {
        direction: [-0.408163, 0.346939, -0.844415],
        wrap: 0.0,
        diffuse: [0.521083, 0.538226, 0.538226],
        specular: [0.599030; 3],
    },
    // The fill: above and to the right, behind the object plane.
    StudioLight {
        direction: [0.521739, 0.826087, -0.212999],
        wrap: 0.478261,
        diffuse: [0.038403, 0.034357, 0.049530],
        specular: [0.106102, 0.125981, 0.158523],
    },
    // The kick: below and to the right, behind.
    StudioLight {
        direction: [0.624519, -0.562067, 0.542269],
        wrap: 0.200000,
        diffuse: [0.090838, 0.082080, 0.072255],
        specular: [0.106535, 0.084771, 0.066080],
    },
];

/// One studio light, in view space: `direction` points toward the light,
/// `wrap` is the terminator softness Blender calls "smooth", and the two
/// colours are the light's diffuse and specular strengths.
#[derive(Debug, Clone, Copy)]
pub struct StudioLight {
    pub direction: [f32; 3],
    pub wrap: f32,
    pub diffuse: [f32; 3],
    pub specular: [f32; 3],
}

/// One studio light turned into model space — the shading math's space, where
/// the normals live.
#[derive(Debug, Clone, Copy)]
pub struct ModelLight {
    pub direction: [f32; 3],
    pub wrap: f32,
    pub diffuse: [f32; 3],
    pub specular: [f32; 3],
}

/// The rig for one camera: the view-space lights ride the camera's basis into
/// model space. The basis is orthonormal, so the inverse is its transpose and
/// the light directions stay unit.
///
/// Anchoring the rig to the camera is Blender's own behaviour with world
/// orientation off: orbiting keeps the key light on the viewer's upper left
/// instead of swinging the model through a fixed beam.
pub fn model_space_lights(framing: &Framing) -> [ModelLight; 4] {
    let mut lights = [ModelLight {
        direction: [0.0; 3],
        wrap: 0.0,
        diffuse: [0.0; 3],
        specular: [0.0; 3],
    }; 4];
    for (slot, light) in STUDIO_LIGHTS.into_iter().enumerate() {
        let direction = add(
            add(
                scale(framing.right, light.direction[0]),
                scale(framing.up, light.direction[1]),
            ),
            scale(framing.forward, light.direction[2]),
        );
        lights[slot] = ModelLight {
            direction: normalize(direction),
            wrap: light.wrap,
            diffuse: light.diffuse,
            specular: light.specular,
        };
    }
    lights
}

/// The studio environment's radiance, in view space.
///
/// This is the "HDRI" the IBL reflects: a soft room — a gradient dome, bright
/// overhead and dim below — plus the four rig lights as soft panels, brighter
/// than their direct share because a reflection is what they exist for. The
/// GPU bakes this into a prefiltered cubemap once at start-up and the shader
/// samples it along the reflection direction, EEVEE's material-preview
/// studio light by way of a function instead of a file.
pub fn environment_radiance(direction: [f32; 3]) -> [f32; 3] {
    let height = (direction[1] * 0.5 + 0.5).clamp(0.0, 1.0);
    let mut sum = [0.05 + 0.30 * height * height; 3];
    for light in STUDIO_LIGHTS {
        let lobe = dot(light.direction, direction).clamp(0.0, 1.0).powf(24.0);
        for (channel, value) in sum.iter_mut().zip(light.specular) {
            *channel += lobe * value * 2.0;
        }
    }
    sum
}

/// Cube face size of the baked environment: the env is smooth, so a small
/// map prefiltered into a mip chain carries it.
pub const ENV_FACE_SIZE: u32 = 32;
/// Mip levels of the prefiltered chain, [`ENV_MIP_MAX_ROUGHNESS`] at the top —
/// EEVEE's `SPHERE_PROBE_MIPMAP_LEVELS` and `SPHERE_PROBE_MIP_MAX_ROUGHNESS`.
pub const ENV_MIP_LEVELS: u32 = 5;
pub const ENV_MIP_MAX_ROUGHNESS: f32 = 0.7;
/// Hammersley samples per prefiltered texel. EEVEE runs 196 for a 2048 map;
/// a 32-face env needs far fewer for the same quality.
const ENV_PREFILTER_SAMPLES: u32 = 64;

/// One prefiltered mip of the environment cubemap: six faces of RGBA radiance
/// (alpha unused), ready for an `Rgba16Float` upload.
#[derive(Debug, Clone)]
pub struct EnvironmentMip {
    pub face_size: u32,
    /// Six faces in wgpu's cube order (+X, -X, +Y, -Y, +Z, -Z), each
    /// `face_size²` RGBA texels, top-to-bottom rows.
    pub faces: Vec<[f32; 4]>,
}

/// The prefiltered environment mip chain, baked once.
///
/// The filter is EEVEE's `mip_convolve` verbatim: each texel averages the
/// sharp environment over a uniform cone whose aperture comes from the
/// roughness this mip stands for, weighted by the spherical Gaussian
/// `exp(2(N·H − 1) / m²)` — the GGX lobe's stand-in — with the cone and the
/// weight both derived from `lod_to_roughness` exactly as the engine's
/// `cone_cosine_from_roughness` and `sample_weight` do.
pub fn environment_mips() -> &'static Vec<EnvironmentMip> {
    static MIPS: std::sync::OnceLock<Vec<EnvironmentMip>> = std::sync::OnceLock::new();
    MIPS.get_or_init(|| {
        let mut mips = Vec::with_capacity(ENV_MIP_LEVELS as usize);
        // Level 0 is the sharp environment itself.
        mips.push(bake_environment_mip(ENV_FACE_SIZE, None));
        for level in 1..ENV_MIP_LEVELS {
            let roughness = lod_to_roughness(level as f32);
            mips.push(bake_environment_mip(
                ENV_FACE_SIZE >> level,
                Some(roughness),
            ));
        }
        mips
    })
}

/// Bake one environment mip: the sharp env at level 0, the SG-cone filtered
/// one above.
fn bake_environment_mip(face_size: u32, roughness: Option<f32>) -> EnvironmentMip {
    let mut faces: Vec<[f32; 4]> = Vec::with_capacity((6 * face_size * face_size) as usize);
    let sample_count = if roughness.is_some() {
        ENV_PREFILTER_SAMPLES
    } else {
        1
    };
    for face in 0..6u32 {
        for y in 0..face_size {
            for x in 0..face_size {
                let center = cube_face_direction(face, x, y, face_size);
                let (radiance, weight) = match roughness {
                    None => (environment_radiance(center), 1.0),
                    Some(roughness) => {
                        let basis = from_up_axis(center);
                        let cone_cos = cone_cosine_from_roughness(roughness);
                        let mut sum = [0.0f32; 3];
                        let mut weight_accum = 0.0f32;
                        for i in 0..sample_count {
                            let rand = hammersley_2d(i, sample_count);
                            let sample = sample_uniform_cone(center, basis, cone_cos, rand);
                            let w = sample_weight(center, sample, roughness);
                            let radiance = environment_radiance(sample);
                            for (channel, value) in sum.iter_mut().zip(radiance) {
                                *channel += w * value;
                            }
                            weight_accum += w;
                        }
                        (scale(sum, 1.0 / weight_accum.max(1e-6)), 1.0)
                    }
                };
                faces.push([radiance[0], radiance[1], radiance[2], weight]);
            }
        }
    }
    EnvironmentMip { face_size, faces }
}

/// The direction one cubemap texel samples, wgpu's cube face order and
/// orientation — the OpenGL table, which is what `texture_cube` sampling
/// answers on every backend wgpu supports.
fn cube_face_direction(face: u32, x: u32, y: u32, size: u32) -> [f32; 3] {
    let u = 2.0 * (x as f32 + 0.5) / size as f32 - 1.0;
    let v = 2.0 * (y as f32 + 0.5) / size as f32 - 1.0;
    match face {
        0 => [1.0, -v, -u],
        1 => [-1.0, -v, u],
        2 => [u, 1.0, v],
        3 => [u, -1.0, -v],
        4 => [u, -v, 1.0],
        _ => [-u, -v, -1.0],
    }
}

/// Linear roughness a prefiltered mip stands for — EEVEE's
/// `lod_to_roughness`, the closed-form inverse of Frostbite's eq. 53 with the
/// 0.4 linear mix that keeps mip 1 from going too sharp.
fn lod_to_roughness(lod: f32) -> f32 {
    let mip_ratio = lod / (ENV_MIP_LEVELS - 1) as f32;
    let a = mip_ratio;
    let b = 0.6f32;
    let c = 0.4f32;
    let ratio =
        (-(4.0 * a * b * c * c + c * c * c * c).sqrt() + 2.0 * a * b + c * c) / (2.0 * b * b);
    (ratio * ENV_MIP_MAX_ROUGHNESS).clamp(0.0, 1.0)
}

/// The cone aperture (as the cosine of the *full* angle) a roughness blurs
/// over — EEVEE's `cone_cosine_from_roughness`, inverting a spherical
/// gaussian chosen so roughness 1 opens to a half-pi cone.
fn cone_cosine_from_roughness(linear_roughness: f32) -> f32 {
    let m = linear_roughness * linear_roughness;
    let cutoff = 0.01 + (0.14 - 0.01) * m;
    let half_angle_cos = 1.0 + (cutoff.ln() * m * m) / 2.0;
    let half_angle_sin_sq = (1.0 - half_angle_cos * half_angle_cos).max(0.0);
    half_angle_cos * half_angle_cos - half_angle_sin_sq
}

/// The spherical-gaussian weight one sample carries — EEVEE's
/// `sample_weight`: `exp(2(N·H − 1) / m²)` with `m` the GGX-mapped roughness.
fn sample_weight(out_direction: [f32; 3], in_direction: [f32; 3], linear_roughness: f32) -> f32 {
    let m = linear_roughness * linear_roughness;
    let half = normalize(add(out_direction, in_direction));
    let nh = dot(out_direction, half).clamp(0.0, 1.0);
    (2.0 * (nh - 1.0) / (m * m)).exp()
}

/// Hammersley point `i` of `n` — the radical-inverse sequence EEVEE's
/// prefilter walks.
fn hammersley_2d(i: u32, n: u32) -> [f32; 2] {
    let mut bits = i;
    let mut v = 0.0f32;
    let mut mask = 1.0f32;
    while bits != 0 {
        v += ((bits & 1) as f32) * mask;
        bits >>= 1;
        mask *= 0.5;
    }
    [i as f32 / n as f32, v]
}

/// An orthonormal basis around `normal`, EEVEE's `from_up_axis`: the axis
/// choice keeps the frame stable across the sphere.
fn from_up_axis(normal: [f32; 3]) -> [[f32; 3]; 3] {
    let up = if normal[2].abs() < 0.9 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let tangent = normalize(cross(up, normal));
    let bitangent = cross(normal, tangent);
    [tangent, bitangent, normal]
}

/// One direction inside the cone around `center`, from a Hammersley pair —
/// EEVEE's `sample_uniform_cone` on the `from_up_axis` basis.
fn sample_uniform_cone(
    center: [f32; 3],
    basis: [[f32; 3]; 3],
    cone_cos: f32,
    rand: [f32; 2],
) -> [f32; 3] {
    let z = cone_cos + (1.0 - cone_cos) * rand[1];
    let phi = rand[0] * std::f32::consts::TAU;
    let r = (1.0 - z * z).max(0.0).sqrt();
    normalize(add(
        add(
            scale(basis[0], r * phi.cos()),
            scale(basis[1], r * phi.sin()),
        ),
        scale(center, z),
    ))
}

/// The environment's first two spherical-harmonic bands, baked once —
/// Blender's basis constants (`0.282094792`, `0.488602512`) and accumulation
/// order, samples weighted by their solid angle. The shader reconstructs the
/// lambert irradiance from these four coefficients, which is the term the
/// roughest reflections mix into (EEVEE's `evaluate_lambert`).
pub fn environment_sh() -> &'static [[f32; 3]; 4] {
    static SH: std::sync::OnceLock<[[f32; 3]; 4]> = std::sync::OnceLock::new();
    SH.get_or_init(|| {
        const SAMPLES: usize = 2048;
        const SOLID_ANGLE: f32 = std::f32::consts::TAU / SAMPLES as f32 * 2.0;
        let (mut m0, mut mn1, mut mm0, mut mp1) = ([0f32; 3], [0f32; 3], [0f32; 3], [0f32; 3]);
        for i in 0..SAMPLES {
            // Fibonacci sphere: even coverage without a random seed.
            let y = 1.0 - 2.0 * (i as f32 + 0.5) / SAMPLES as f32;
            let radius = (1.0 - y * y).max(0.0).sqrt();
            let phi = std::f32::consts::PI * (1.0 + 2.0 * i as f32);
            let direction = [radius * phi.cos(), radius * phi.sin(), y];
            let radiance = environment_radiance(direction);
            let weight = SOLID_ANGLE;
            for (channel, value) in m0.iter_mut().zip(radiance) {
                *channel += 0.282_094_8 * value * weight;
            }
            for (channel, value) in mn1.iter_mut().zip(radiance) {
                *channel += -0.488_602_5 * direction[1] * value * weight;
            }
            for (channel, value) in mm0.iter_mut().zip(radiance) {
                *channel += 0.488_602_5 * direction[2] * value * weight;
            }
            for (channel, value) in mp1.iter_mut().zip(radiance) {
                *channel += -0.488_602_5 * direction[0] * value * weight;
            }
        }
        [m0, mn1, mm0, mp1]
    })
}

/// Edge of the split-sum environment BRDF table. The function it holds is
/// smooth in both axes, so 128 texels with bilinear filtering carry it; a
/// 512² table would cost sixteen times the bake for detail nobody could see.
pub const BRDF_LUT_SIZE: u32 = 128;
/// Importance samples per texel of the bake. Filament runs 512 for a 512²
/// table; 256 over a smooth 128² field lands the same visual answer.
const BRDF_LUT_SAMPLES: u32 = 256;

/// The split-sum environment BRDF table: the pre-integrated Fresnel/geometry
/// response of a GGX surface against the environment, indexed by `u` = the
/// view incidence `NoV` and `v` = the linear roughness.
///
/// This is EEVEE's split-sum lightprobe eval: the environment's specular
/// contribution is `radiance · (F0 · table.x + table.y)` — the integral of
/// the GGX Fresnel and geometry terms over the hemisphere, precomputed once
/// (Karis, "Real Shading in Unreal Engine 4", split-sum 2; the integral is
/// Filament's `prefilterDfg` with the height-correlated Smith visibility).
/// The CPU bakes the table and samples it bilinearly; the GPU uploads the
/// same table and lets the sampler do it — one table, two readers, so a
/// thumbnail's reflections and the viewport's agree.
#[derive(Debug, Clone)]
pub struct BrdfLut {
    pub size: u32,
    /// `[scale, bias]` per texel, row-major: `y` is the roughness row.
    pub data: Vec<[f32; 2]>,
}

impl BrdfLut {
    /// The table's pair at `(no_v, roughness)`, bilinear over the texel
    /// centres. Mirrors what the GPU's clamped linear sampler produces for
    /// the same coordinates, so the two renderers cannot disagree.
    pub fn sample(&self, no_v: f32, roughness: f32) -> [f32; 2] {
        let size = self.size as f32;
        // Texel coordinates: the bake centres texel i at (i + 0.5) / size,
        // which is exactly where a clamped linear sampler reads uv.
        let x = no_v.clamp(0.0, 1.0) * size - 0.5;
        let y = roughness.clamp(0.0, 1.0) * size - 0.5;
        let last = self.size as i32 - 1;
        let x0 = (x.floor() as i32).clamp(0, last);
        let y0 = (y.floor() as i32).clamp(0, last);
        let x1 = (x0 + 1).min(last);
        let y1 = (y0 + 1).min(last);
        let fx = (x - x0 as f32).clamp(0.0, 1.0);
        let fy = (y - y0 as f32).clamp(0.0, 1.0);
        let at = |tx: i32, ty: i32| self.data[(ty * self.size as i32 + tx) as usize];
        let top = [
            at(x0, y0)[0] + (at(x1, y0)[0] - at(x0, y0)[0]) * fx,
            at(x0, y0)[1] + (at(x1, y0)[1] - at(x0, y0)[1]) * fx,
        ];
        let bottom = [
            at(x0, y1)[0] + (at(x1, y1)[0] - at(x0, y1)[0]) * fx,
            at(x0, y1)[1] + (at(x1, y1)[1] - at(x0, y1)[1]) * fx,
        ];
        [
            top[0] + (bottom[0] - top[0]) * fy,
            top[1] + (bottom[1] - top[1]) * fy,
        ]
    }
}

/// Bake the split-sum environment BRDF table, once for the process.
pub fn environment_brdf_lut() -> &'static BrdfLut {
    static LUT: std::sync::OnceLock<BrdfLut> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        use rayon::prelude::*;

        let size = BRDF_LUT_SIZE as usize;
        let rows: Vec<Vec<[f32; 2]>> = (0..size)
            .into_par_iter()
            .map(|iy| {
                // The row's roughness, at the texel centre.
                let roughness = (iy as f32 + 0.5) / BRDF_LUT_SIZE as f32;
                (0..size)
                    .map(|ix| {
                        let no_v = ((ix as f32 + 0.5) / BRDF_LUT_SIZE as f32).max(1e-4);
                        bake_brdf_texel(no_v, roughness)
                    })
                    .collect()
            })
            .collect();
        BrdfLut {
            size: BRDF_LUT_SIZE,
            data: rows.into_iter().flatten().collect(),
        }
    })
}

/// Integrate one table texel: the GGX importance-sampled average of the
/// Fresnel split against the Smith geometry term, Filament's `prefilterDfg`.
fn bake_brdf_texel(no_v: f32, roughness: f32) -> [f32; 2] {
    // Looking down +Z at the surface; the L is the reflection of V about each
    // sampled half-vector.
    let v = [(1.0 - no_v * no_v).max(0.0).sqrt(), 0.0, no_v];
    let alpha = roughness * roughness;
    let a2 = alpha * alpha;
    let mut scale = 0.0f32;
    let mut bias = 0.0f32;
    for i in 0..BRDF_LUT_SAMPLES {
        let xi = hammersley_2d(i, BRDF_LUT_SAMPLES);
        let phi = std::f32::consts::TAU * xi[0];
        // GGX NDF sampling: the half-vector's angle from the normal.
        let cos_theta = ((1.0 - xi[1]) / (1.0 + (a2 - 1.0) * xi[1])).max(0.0).sqrt();
        let sin_theta = (1.0 - cos_theta * cos_theta).max(0.0).sqrt();
        let h = [sin_theta * phi.cos(), sin_theta * phi.sin(), cos_theta];
        let voh = dot(v, h).clamp(0.0, 1.0);
        let l = [
            2.0 * voh * h[0] - v[0],
            2.0 * voh * h[1] - v[1],
            2.0 * voh * h[2] - v[2],
        ];
        let no_l = l[2].clamp(0.0, 1.0);
        if no_l <= 0.0 {
            continue;
        }
        let no_h = h[2].clamp(0.0, 1.0);
        // Height-correlated Smith visibility with the `VoH / NoH` factor the
        // split-sum integral folds in, times the four the NDF-sampled Monte
        // Carlo carries (Filament's `r * 4 / sampleCount`) — without it a
        // perfect mirror would read 0.25 instead of passing the environment
        // through at its Fresnel weight.
        let lambda_v = no_l
            * ((no_v - a2 * no_v) * (no_v - a2 * no_v) + a2)
                .max(0.0)
                .sqrt();
        let lambda_l = no_v
            * ((no_l - a2 * no_l) * (no_l - a2 * no_l) + a2)
                .max(0.0)
                .sqrt();
        let v_pdf = 2.0 / (lambda_v + lambda_l) * voh / no_h.max(1e-4);
        let fresnel = (1.0 - voh).powi(5);
        scale += (1.0 - fresnel) * v_pdf;
        bias += fresnel * v_pdf;
    }
    let count = BRDF_LUT_SAMPLES as f32;
    // The NoV→0, roughness→0 corner's estimator is unbounded — a razor-thin
    // lobe seen edge-on — and no display-visible weight needs more than a
    // few times one, so the table clamps there instead of carrying an
    // enormous corner the sampler would smear into its neighbourhood.
    [(scale / count).min(4.0), (bias / count).min(4.0)]
}

/// How rough the studio lighting shades the surface: Blender's fresh Principled
/// BSDF, a medium-rough dielectric. A file's glTF factors are already flattened
/// into its vertex colours, and without its textures the per-material roughness
/// is usually the default anyway, so the lighting reads this until materials
/// carry their own roughness. Metals are out with it: a metal's specular is
/// its base colour, which the flattened vertices do not keep separate.
pub const MATERIAL_ROUGHNESS: f32 = 0.5;
/// Dielectric reflectance at normal incidence — where a non-metal's specular
/// colour starts.
pub const DIELECTRIC_F0: f32 = 0.05;

/// Background gradient: brighter at the top, with a soft corner vignette.
pub const BG_TOP: [f32; 3] = [0.965, 0.969, 0.976];
pub const BG_BOTTOM: [f32; 3] = [0.869, 0.882, 0.898];
/// The surface a file without materials wears: a neutral grey, sitting well
/// under the light background so the model reads against it. It used to be a
/// "cool" grey — blue by a blue/red ratio of 1.18 — which every material-less
/// OBJ, STL or PLY took wholesale, and which read as a blue cast over the
/// whole model; the studio rig itself is near-neutral, so the colour the
/// model wears is the colour the viewer sees.
pub const MATERIAL: [f32; 3] = [0.640, 0.640, 0.640];
/// Fraction of the image the corner vignette darkens by.
pub const VIGNETTE: f32 = 0.35;
/// Radius of one point sprite, in pixels of the final image. Both renderers
/// draw a cloud as discs of this size; it is a uniform because the GPU shader
/// bakes in no constants of its own.
pub const POINT_RADIUS: f32 = 1.15;

/// The three axis colours, the usual red/green/blue: X, Y, Z. The viewport's
/// pivot symbol paints its three rings with these, so which ring belongs to
/// which axis reads the same way it does in CloudCompare.
pub const AXIS_X: [f32; 3] = [0.87, 0.28, 0.28];
pub const AXIS_Y: [f32; 3] = [0.30, 0.74, 0.34];
pub const AXIS_Z: [f32; 3] = [0.30, 0.47, 0.90];
/// Amber for the centre of the three axes, standing apart from the three axis
/// colours.
pub const AXIS_ORIGIN: [f32; 3] = [0.95, 0.76, 0.25];

/// An orbiting camera aimed at the centre of the model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// Rotation around the model's up axis, in radians.
    pub yaw: f32,
    /// Elevation in radians, clamped to ±[`MAX_PITCH`].
    pub pitch: f32,
    /// Distance multiplier; 1.0 frames the whole model.
    pub zoom: f32,
    /// Pivot offset across the view, in bounding-sphere radii: `x` along the
    /// view's right axis, `y` along its up axis. The eye travels with the
    /// pivot, so orbiting after a pan still turns around what the user is
    /// looking at — the difference between panning a view and sliding a
    /// picture inside a fixed one.
    pub pan: [f32; 2],
}

impl Default for Camera {
    /// A three-quarter view, which shows both the top and one side.
    fn default() -> Self {
        Self {
            yaw: 0.62,
            pitch: 0.34,
            zoom: 1.0,
            pan: [0.0, 0.0],
        }
    }
}

impl Camera {
    /// Turn the camera by a delta in radians, wrapping yaw and clamping pitch.
    pub fn orbit(&mut self, delta_yaw: f32, delta_pitch: f32) {
        self.yaw = wrap_angle(self.yaw + delta_yaw);
        self.pitch = (self.pitch + delta_pitch).clamp(-MAX_PITCH, MAX_PITCH);
    }

    /// Scale the distance; a factor below 1 moves closer. The caller
    /// supplies the limits so they can come from the app config.
    pub fn zoom_by(&mut self, factor: f32, min_zoom: f32, max_zoom: f32) {
        self.zoom = (self.zoom * factor).clamp(min_zoom, max_zoom);
    }

    /// Slide the pivot by a delta across the view, in bounding-sphere radii:
    /// positive `x` right, positive `y` up.
    pub fn pan_by(&mut self, delta: [f32; 2]) {
        self.pan = [self.pan[0] + delta[0], self.pan[1] + delta[1]];
    }

    /// How much of the view one pixel of drag covers at the pivot, in
    /// bounding-sphere radii.
    ///
    /// Measured where the model is, so a panned or zoomed view moves the same
    /// distance under the cursor as the pixels the cursor travelled — the
    /// property that makes dragging feel like grabbing the model.
    pub fn pan_per_pixel(&self, viewport_height: f32) -> f32 {
        let distance = fit_distance() * self.zoom;
        let tan_half = (FOV_DEG.to_radians() * 0.5).tan();
        2.0 * distance * tan_half / viewport_height.max(1.0)
    }

    /// Back to the default three-quarter view.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether the camera is already in its default pose.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A rendered frame: BGRA bytes, four per pixel, top row first.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, in BGRA order.
    pub bgra: Vec<u8>,
}

impl Frame {
    /// The BGRA bytes of one pixel. Panics when out of range.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) as usize) * 4;
        [
            self.bgra[i],
            self.bgra[i + 1],
            self.bgra[i + 2],
            self.bgra[i + 3],
        ]
    }

    /// Whether nothing was drawn, i.e. every pixel is still background.
    ///
    /// The green channel is the discriminator: background green never drops
    /// below 225, while the brightest shaded pixel stays under 216.
    pub fn is_empty_of_geometry(&self) -> bool {
        self.bgra.as_chunks::<4>().0.iter().all(|p| p[1] >= 220)
    }
}

/// Clamp a requested edge length into the renderable range.
pub fn clamp_edge(edge: u32) -> u32 {
    edge.clamp(MIN_EDGE, MAX_EDGE)
}

/// Render `mesh` from `camera` into a `width`×`height` BGRA frame.
///
/// `supersample` (1 or 2) trades time for smoother silhouettes: 2 renders at
/// twice the resolution and box-filters down, which costs four times the work.
/// The interactive draft frame uses 1, the idle frame 2.
///
/// `quality` (0..=1] is the fraction of the geometry to actually rasterise:
/// `1.0` draws every triangle or point, lower values draw every Nth one.
/// Used to keep interaction with a heavy mesh responsive — a quarter of the
/// triangles a quarter of the time still reads as the same model turning.
pub fn render(
    mesh: &Mesh,
    camera: &Camera,
    width: u32,
    height: u32,
    supersample: u32,
    quality: f32,
) -> Frame {
    render_with_scratch(
        mesh,
        camera,
        width,
        height,
        supersample,
        quality,
        RenderOptions::default(),
        &mut Scratch::default(),
    )
}

/// Scratch buffers a rasteriser needs, kept between frames.
///
/// A frame is `width × height × 16` bytes of colour and depth plus one
/// position pair per vertex — on a 1 MP preview and a million-vertex mesh
/// that is over 50 MB of allocation per frame, for buffers whose size never
/// changes while the viewport does. Holding them makes an interactive drag
/// pay for pixels instead of for the allocator.
#[derive(Debug, Default, Clone)]
pub struct Scratch {
    colors: Vec<[f32; 3]>,
    depth: Vec<f32>,
    /// Model-space and view-space positions of every vertex, one pass each.
    model: Vec<[f32; 3]>,
    view: Vec<[f32; 3]>,
    /// Per-pixel `-log2(depth)`, which is what eye-dome lighting compares.
    log_depth: Vec<f32>,
    /// The key light's depth map, reversed-Z (`0` = nothing drawn): rasterised
    /// once per frame from the light's orthographic view, then sampled with a
    /// 3×3 PCF by every shaded pixel.
    shadow: Vec<f32>,
}

impl Scratch {
    /// Bytes currently held, for the viewport's own accounting and tests.
    pub fn bytes(&self) -> usize {
        self.colors.len() * std::mem::size_of::<[f32; 3]>()
            + self.depth.len() * std::mem::size_of::<f32>()
            + (self.model.len() + self.view.len()) * std::mem::size_of::<[f32; 3]>()
            + (self.log_depth.len() + self.shadow.len()) * std::mem::size_of::<f32>()
    }
}

/// Optional work the rasteriser can do on top of a raw frame.
///
/// Everything defaults to off except `material_colors` — so [`render`] — and
/// every caller that did not ask keeps producing exactly the frame it always
/// did, with the file's own materials showing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderOptions {
    /// Skip the back faces of a closed, outward-wound mesh.
    ///
    /// Safe only for a mesh whose [`Mesh::winding`] says so: on an open shell
    /// the back face is the surface you see from the other side, and skipping
    /// it would punch a hole in the model.
    pub cull_backfaces: bool,
    /// Eye-dome lighting and gap filling for point clouds.
    ///
    /// A cloud is drawn as discs of a couple of pixels; without this, sparse
    /// regions read as dust and the shape of the surface inside is hard to
    /// see. Eye-dome lighting darkens where the cloud turns away from the
    /// camera, which is what makes the form legible; gap filling closes the
    /// single-pixel holes between neighbouring discs.
    pub enhance_points: bool,
    /// Whether a mesh is painted with the per-vertex colours its file
    /// declared — its materials. The one option whose default is on: the file
    /// meant those colours, and thumbnails render through
    /// [`RenderOptions::default`], where a card should say what the file
    /// looks like. Point clouds keep their own colours either way; those are
    /// scan data, not materials.
    pub material_colors: bool,
    /// Whether the key studio light casts a shadow map and the surface
    /// tests against it. Self-shadowing is what folds a model into shape —
    /// creases, overhangs, the contact an arm makes with a torso — so the
    /// default is on, and with it off the rasteriser leaves the shadow map
    /// empty, which every pixel tests out of (fully lit). Point clouds are
    /// never shadowed either way: their eye-dome lighting is what gives them
    /// form.
    pub shadows: bool,
    /// How the surface is painted by height, rather than by the material.
    ///
    /// How the surface is painted by its field values, already measured against
    /// the range the caller chose. [`HeightMode::Off`](crate::media::height_color::HeightMode)
    /// — the default — keeps the flat material for a mesh and the file's own
    /// colours for a cloud, so nothing that did not ask gets a look change.
    pub height: HeightField,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            cull_backfaces: false,
            enhance_points: false,
            material_colors: true,
            shadows: true,
            height: HeightField::default(),
        }
    }
}

/// [`render`], reusing the caller's buffers. The interactive path goes through
/// this one.
///
/// The frame size, the two quality knobs, the options and the scratch buffers
/// are independent of one another, and bundling them would only move the list
/// somewhere else — the viewport already groups callers by frame kind.
#[allow(clippy::too_many_arguments)]
pub fn render_with_scratch(
    mesh: &Mesh,
    camera: &Camera,
    width: u32,
    height: u32,
    supersample: u32,
    quality: f32,
    options: RenderOptions,
    scratch: &mut Scratch,
) -> Frame {
    let width = clamp_edge(width);
    let height = clamp_edge(height);
    let ss = supersample.clamp(1, MAX_SUPERSAMPLE);
    let quality = quality.clamp(0.05, 1.0);
    // Sprites are sized in final-image pixels, so the supersampled buffer
    // needs them scaled up or a cloud would come out thinner after the filter.
    paint(
        mesh,
        camera,
        width * ss,
        height * ss,
        POINT_RADIUS * ss as f32,
        quality,
        options,
        scratch,
    );
    if options.enhance_points && mesh.is_point_cloud() {
        // At the supersampled resolution, before the box filter: the lighting
        // then survives the downsample instead of being averaged away.
        enhance_points(scratch, (width * ss) as usize, (height * ss) as usize);
    }
    let bgra = if ss > 1 {
        let down = downsample(
            &scratch.colors,
            (width * ss) as usize,
            (height * ss) as usize,
            width as usize,
            height as usize,
        );
        to_bgra(&down)
    } else {
        to_bgra(&scratch.colors)
    };
    Frame {
        width,
        height,
        bgra,
    }
}

/// Longest edge, in pixels, at which to render a model: heavy models get fewer
/// pixels so dragging stays responsive. `max_edge` is the ideal size.
///
/// `primitives` is triangles for a mesh, points for a cloud — see
/// [`Mesh::primitive_count`].
pub fn auto_size(primitives: usize, max_edge: u32) -> u32 {
    let cap = match primitives {
        0..=40_000 => max_edge,
        40_001..=120_000 => max_edge.min(900),
        120_001..=300_000 => max_edge.min(720),
        _ => max_edge.min(560),
    };
    cap.max(AUTO_SIZE_FLOOR.min(max_edge)).min(max_edge)
}

/// A camera resolved against a particular model's bounds.
///
/// Both renderers go through this: the CPU rasterizer uses the basis vectors
/// and [`Framing::to_screen`] directly, while the GPU backend uploads
/// [`Framing::view_projection`] as a uniform. Sharing it is what keeps a model
/// framed identically whichever backend drew it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Framing {
    /// The point the camera looks at, in file coordinates: the model's
    /// bounding-sphere centre, moved by the camera's pan.
    pub center: [f32; 3],
    /// Reciprocal of the bounding-sphere radius: model space → unit sphere.
    pub inv_radius: f32,
    /// Camera position in unit-sphere space.
    pub eye: [f32; 3],
    pub right: [f32; 3],
    pub up: [f32; 3],
    /// Points from the eye into the scene.
    pub forward: [f32; 3],
    /// Eye distance from the centre, in unit-sphere space.
    pub distance: f32,
    /// `tan(fov / 2)`.
    pub tan_half: f32,
    /// Viewport aspect ratio (width / height).
    pub aspect: f32,
}

impl Framing {
    /// Model space → unit-sphere space, which is where the eye lives.
    pub fn to_unit(&self, p: [f32; 3]) -> [f32; 3] {
        [
            (p[0] - self.center[0]) * self.inv_radius,
            (p[1] - self.center[1]) * self.inv_radius,
            (p[2] - self.center[2]) * self.inv_radius,
        ]
    }

    /// Model space → view space: x right, y up, z forward.
    pub fn to_view(&self, p: [f32; 3]) -> [f32; 3] {
        let d = sub(self.to_unit(p), self.eye);
        [dot(d, self.right), dot(d, self.up), dot(d, self.forward)]
    }

    /// View space → pixel coordinates (top-left origin) plus `1/z`, which is
    /// linear in screen space and grows as the point comes nearer.
    pub fn to_screen(&self, view: [f32; 3], width: f32, height: f32) -> ([f32; 2], f32) {
        let inv_z = 1.0 / view[2];
        let ndc_x = view[0] * inv_z / (self.tan_half * self.aspect);
        let ndc_y = view[1] * inv_z / self.tan_half;
        (
            [(ndc_x * 0.5 + 0.5) * width, (0.5 - ndc_y * 0.5) * height],
            inv_z,
        )
    }

    /// The near plane in unit-sphere space, and the far distance the CPU
    /// rasteriser's scratch math used to clip against. The projection itself
    /// has an infinite far plane (see [`Self::view_projection`]); the far
    /// value stays for the CPU paths and the eye-dome constants.
    pub fn depth_range(&self) -> (f32, f32) {
        ((self.distance * 0.01).max(1e-5), self.distance + 2.0)
    }

    /// The camera's position back in file coordinates, for shading (the GPU
    /// fragment stage lights in model space, where the normals live).
    pub fn eye_in_model_space(&self) -> [f32; 3] {
        let radius = 1.0 / self.inv_radius;
        [
            self.center[0] + self.eye[0] * radius,
            self.center[1] + self.eye[1] * radius,
            self.center[2] + self.eye[2] * radius,
        ]
    }

    /// Model space → clip space, **column-major**, ready to upload as a WGSL
    /// `mat4x4<f32>`. Depth is *reversed* with an infinite far plane — the
    /// stored value is `near / vz`, `1` at the near plane falling towards `0`
    /// — the projection Blender's `projection::perspective_infinite` builds
    /// (Lengyel, "Projection Matrix Tricks", GDC 2007).
    ///
    /// Reversed-Z exists for the depth buffer's precision: a fixed-point
    /// depth distributed hyperbolically loses ~50 model units of resolution
    /// at this scene's scale, which lets a box sitting *behind* the screen
    /// quad of a model poke through it as a black patch. Storing `near / vz`
    /// in a float32 buffer keeps the precision relative to the distance
    /// instead — sub-millimetre here — and the compare flips to
    /// GreaterEqual with a clear of 0 to match.
    pub fn view_projection(&self) -> [[f32; 4]; 4] {
        let near = self.depth_range().0;
        // x_clip = a * vx, y_clip = b * vy, w_clip = vz, and z_clip is the
        // constant near: the perspective divide lands the depth on
        // `near / vz`, exactly the reversed curve above. A constant row is
        // as interpolable as any affine row — w_clip carries the division.
        let a = 1.0 / (self.tan_half * self.aspect);
        let b = 1.0 / self.tan_half;

        let row = |axis: [f32; 3], gain: f32| -> [f32; 4] {
            // m = (p - center) * inv_radius, then dot with `axis` and offset
            // by the eye's projection onto the same axis.
            let origin = -gain * (dot(self.eye, axis) + self.inv_radius * dot(self.center, axis));
            [
                axis[0] * gain * self.inv_radius,
                axis[1] * gain * self.inv_radius,
                axis[2] * gain * self.inv_radius,
                origin,
            ]
        };

        let x = row(self.right, a);
        let y = row(self.up, b);
        let w = row(self.forward, 1.0);
        let z = [0.0, 0.0, 0.0, near];

        // `[[f32; 4]; 4]` is indexed [row][column]; WGSL reads mat4x4 as four
        // consecutive column vectors, so transpose on the way out. The column
        // order has to spell the clip vector `(x, y, z, w)`: putting `w` in
        // the third slot would hand the shader a swapped depth and divisor.
        [
            [x[0], y[0], z[0], w[0]],
            [x[1], y[1], z[1], w[1]],
            [x[2], y[2], z[2], w[2]],
            [x[3], y[3], z[3], w[3]],
        ]
    }

    /// This camera's frustum in **model space**, for culling geometry before
    /// it is loaded or drawn.
    ///
    /// The extraction reads the matrix by rows and [`Framing::view_projection`]
    /// hands it over by columns, so the transpose happens here rather than in
    /// every caller: getting that backwards produces a plausible-looking
    /// frustum that culls the wrong half of the model.
    pub fn frustum(&self) -> Frustum {
        let columns = self.view_projection();
        let mut rows = [[0.0f32; 4]; 4];
        for (column, values) in columns.iter().enumerate() {
            for (row, value) in values.iter().enumerate() {
                rows[row][column] = *value;
            }
        }
        Frustum::from_matrix(&rows)
    }
}

impl Camera {
    /// Resolve this camera against `bounds` for a viewport of `aspect`.
    pub fn framing(&self, bounds: Bounds, aspect: f32) -> Framing {
        let center = bounds.center();
        let size = bounds.size();
        let radius =
            (0.5 * (size[0] * size[0] + size[1] * size[1] + size[2] * size[2]).sqrt()).max(1e-6);
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        let to_eye = [cos_pitch * sin_yaw, sin_pitch, cos_pitch * cos_yaw];
        let distance = fit_distance() * self.zoom;
        let forward = neg(to_eye);
        let right = normalize(cross(forward, [0.0, 1.0, 0.0]));
        let up = cross(right, forward);
        // Panning moves the pivot and the eye together, so the model slides
        // across the viewport and a later orbit still turns around whatever
        // the user panned to.
        let pivot = add(
            center,
            add(
                scale(right, self.pan[0] * radius),
                scale(up, self.pan[1] * radius),
            ),
        );
        Framing {
            center: pivot,
            inv_radius: 1.0 / radius,
            eye: scale(to_eye, distance),
            right,
            up,
            forward,
            distance,
            tan_half: (FOV_DEG.to_radians() * 0.5).tan(),
            aspect,
        }
    }
}

/// Mesh data laid out for a GPU vertex buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct VertexData {
    /// Interleaved `[x, y, z, nx, ny, nz]` per vertex.
    pub vertices: Vec<f32>,
    /// Per-vertex `[r, g, b]`, only when the mesh carries its own colours —
    /// the file's material or vertex-colour attributes. Empty otherwise, which
    /// puts the mesh on the plain pipeline and its flat material.
    pub colors: Vec<f32>,
    /// Per-vertex `[u, v, layer]`, only when the mesh carries base-colour
    /// textures: the UV to sample and which texture of the model's set to
    /// sample. Empty otherwise. Layer `-1.0` marks a vertex with no texture,
    /// which the shader answers with white.
    pub tex: Vec<f32>,
    /// Triangle indices, or `None` when the mesh had to be expanded per face.
    pub indices: Option<Vec<u32>>,
    /// Vertices in the buffer, i.e. `vertices.len() / 6`.
    pub vertex_count: u32,
}

impl VertexData {
    /// Bytes of one interleaved vertex: two `vec3<f32>`.
    pub const STRIDE: u64 = 24;

    /// Bytes of one colour vertex: a `vec3<f32>`.
    pub const COLOR_STRIDE: u64 = 12;
    /// Bytes of one
    /// `[u, v, base layer, mr layer, metallic f, roughness f, normal layer, ao layer, normal scale, ao strength, emissive layer, emissive r, g, b, alpha cutoff, alpha factor, double sided, 0]`
    /// record.
    pub const TEX_STRIDE: u64 = 72;

    /// The texture coordinates as bytes, ready for `Queue::write_buffer`.
    pub fn tex_bytes(&self) -> Vec<u8> {
        f32_bytes(&self.tex)
    }

    /// Triangles that will be drawn.
    pub fn triangle_count(&self) -> usize {
        match &self.indices {
            Some(index) => index.len() / 3,
            None => self.vertex_count as usize / 3,
        }
    }

    /// The vertex array as bytes, ready for `Queue::write_buffer`.
    pub fn vertex_bytes(&self) -> Vec<u8> {
        f32_bytes(&self.vertices)
    }

    /// The colour array as bytes, ready for `Queue::write_buffer`.
    pub fn color_bytes(&self) -> Vec<u8> {
        f32_bytes(&self.colors)
    }

    /// The index list as bytes, when the mesh is indexed.
    pub fn index_bytes(&self) -> Option<Vec<u8>> {
        self.indices.as_ref().map(|indices| u32_bytes(indices))
    }
}

/// Reinterpret floats as the bytes a GPU buffer takes them as.
///
/// Host-endian on purpose: a wgpu buffer holds the host's own representation,
/// and this avoids an `unsafe` cast or a bytemuck dependency for four lines.
pub fn f32_bytes(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_ne_bytes());
    }
    out
}

/// Reinterpret `u32`s as bytes, the index-buffer counterpart of [`f32_bytes`].
pub fn u32_bytes(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_ne_bytes());
    }
    out
}

/// Flatten a mesh into an interleaved vertex array for a GPU buffer.
///
/// A mesh that carries normals keeps its vertices and index list, so the
/// fragment stage can interpolate for smooth shading. One that does not is
/// expanded triangle by triangle with the face normal repeated on all three
/// corners, which reproduces the CPU rasterizer's flat shading.
pub fn vertex_data(mesh: &Mesh) -> VertexData {
    vertex_data_with(mesh, false)
}

/// [`vertex_data`], optionally reversing every triangle's winding.
///
/// A closed mesh wound inside-out is easiest to fix here, on the way into the
/// buffer, rather than by cloning the geometry: reversing the corner order
/// both points the index list the right way and, on the flat-shaded path,
/// flips the face normal that is computed from it. That lets the renderer
/// cull back faces of a correctly wound surface whichever way the file
/// spelled it.
pub fn vertex_data_with(mesh: &Mesh, flip_winding: bool) -> VertexData {
    let mut vertices = Vec::new();
    // The file's own colours ride along only when it has them: an empty array
    // is what puts the mesh on the plain pipeline and its flat material.
    let mut colors = Vec::new();
    let colored = mesh.has_vertex_colors();
    if colored {
        colors.reserve(mesh.positions.len() * 3);
    }
    // The same for the texture coordinates and the material's PBR factors,
    // packed as `[u, v, base layer, mr layer, metallic f, roughness f,
    // normal layer, ao layer, normal scale, ao strength, emissive layer,
    // emissive rgb, alpha cutoff, alpha factor]` per vertex. Layer `0` is the
    // white stand-in the shader answers when the primitive carries no such
    // texture; the real slots shift up by one. The factors default to the
    // glTF spec's (1.0, 1.0), so the white layer's `g = b = 1` samples leave
    // the spec defaults standing.
    let mut tex = Vec::new();
    let textured = mesh.has_textures();
    if textured {
        tex.reserve(mesh.positions.len() * 18);
    }
    let tex_at = |index: usize| -> [f32; 18] {
        let data = mesh.texture.as_ref().expect("textured");
        let slot = data.slot.get(index).copied().unwrap_or(NO_TEXTURE);
        let mr_slot = data.mr_slot.get(index).copied().unwrap_or(NO_TEXTURE);
        let normal_slot = data.normal_slot.get(index).copied().unwrap_or(NO_TEXTURE);
        let ao_slot = data.ao_slot.get(index).copied().unwrap_or(NO_TEXTURE);
        let emissive_slot = data.emissive_slot.get(index).copied().unwrap_or(NO_TEXTURE);
        let uv = data.uv.get(index).copied().unwrap_or([0.0, 0.0]);
        let factors = data.factors.get(index).copied().unwrap_or([0.0, 0.0]);
        let normal_scale = data.normal_scale.get(index).copied().unwrap_or(1.0);
        let ao_strength = data.ao_strength.get(index).copied().unwrap_or(1.0);
        let emissive = data.emissive_factor.get(index).copied().unwrap_or([0.0; 3]);
        [
            uv[0],
            uv[1],
            if slot == NO_TEXTURE {
                0.0
            } else {
                slot as f32 + 1.0
            },
            if mr_slot == NO_TEXTURE {
                0.0
            } else {
                mr_slot as f32 + 1.0
            },
            factors[0],
            factors[1],
            if normal_slot == NO_TEXTURE {
                0.0
            } else {
                normal_slot as f32 + 1.0
            },
            if ao_slot == NO_TEXTURE {
                0.0
            } else {
                ao_slot as f32 + 1.0
            },
            normal_scale,
            ao_strength,
            if emissive_slot == NO_TEXTURE {
                0.0
            } else {
                emissive_slot as f32 + 1.0
            },
            emissive[0],
            emissive[1],
            emissive[2],
            // Negative cutoff = opaque: the shader tests only when it is set.
            data.alpha_cutoff.get(index).copied().unwrap_or(-1.0),
            data.alpha_factor.get(index).copied().unwrap_or(1.0),
            // The material's doubleSided, as the flag the shader's fragment
            // test reads; the trailing zero pads the vec4.
            if data.double_sided.get(index).copied().unwrap_or(false) {
                1.0
            } else {
                0.0
            },
            0.0,
        ]
    };
    if mesh.has_vertex_normals() {
        vertices.reserve(mesh.positions.len() * 6);
        for (index, (p, n)) in mesh.positions.iter().zip(mesh.normals.iter()).enumerate() {
            // A flipped winding means the file's normals point the other way
            // too, or the shading would disagree with the culling.
            let n = if flip_winding { neg(*n) } else { *n };
            let n = normalize(n);
            vertices.extend_from_slice(&[p[0], p[1], p[2], n[0], n[1], n[2]]);
            if colored {
                let c = base_color(mesh, index);
                colors.extend_from_slice(&[c[0], c[1], c[2]]);
            }
            if textured {
                let t = tex_at(index);
                tex.extend_from_slice(&t);
            }
        }
        let mut indices = Vec::with_capacity(mesh.triangles.len() * 3);
        for triangle in &mesh.triangles {
            if flip_winding {
                indices.extend_from_slice(&[triangle[0], triangle[2], triangle[1]]);
            } else {
                indices.extend_from_slice(triangle);
            }
        }
        VertexData {
            vertices,
            colors,
            tex,
            indices: Some(indices),
            vertex_count: mesh.positions.len() as u32,
        }
    } else {
        vertices.reserve(mesh.triangles.len() * 18);
        for triangle in &mesh.triangles {
            // The expanded corners follow the winding, and so do their colours:
            // the same slot indexes the same vertex of the source triangle.
            let source = if flip_winding {
                [triangle[0], triangle[2], triangle[1]]
            } else {
                *triangle
            };
            let corners = [
                mesh.positions[source[0] as usize],
                mesh.positions[source[1] as usize],
                mesh.positions[source[2] as usize],
            ];
            let n = normalize(cross(
                sub(corners[1], corners[0]),
                sub(corners[2], corners[0]),
            ));
            for (slot, p) in corners.iter().enumerate() {
                vertices.extend_from_slice(&[p[0], p[1], p[2], n[0], n[1], n[2]]);
                if colored {
                    let c = base_color(mesh, source[slot] as usize);
                    colors.extend_from_slice(&[c[0], c[1], c[2]]);
                }
                if textured {
                    let t = tex_at(source[slot] as usize);
                    tex.extend_from_slice(&t);
                }
            }
        }
        VertexData {
            vertex_count: (mesh.triangles.len() * 3) as u32,
            vertices,
            colors,
            tex,
            indices: None,
        }
    }
}

/// A point cloud laid out for a GPU instance buffer.
///
/// One instance per point, drawn as a camera-facing sprite: the vertex buffer
/// holds the sprites' corners (generated in the shader) and this holds the
/// points themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct PointData {
    /// Interleaved `[x, y, z, nx, ny, nz, r, g, b, intensity, class]` per point.
    pub points: Vec<f32>,
    /// Points in the buffer, i.e. `points.len() / 11`.
    pub count: u32,
}

impl PointData {
    /// Bytes of one interleaved point: position, normal, base colour and the two
    /// scalar channels.
    ///
    /// The channels are two separate floats rather than a `vec2` because a vertex
    /// attribute's offset must be a multiple of its format's size, and the tenth
    /// float sits at byte 36, which no two-component attribute can start at.
    pub const STRIDE: u64 = 3 * 3 * 4 + 2 * 4;

    /// The point array as bytes, ready for `Queue::write_buffer`.
    pub fn bytes(&self) -> Vec<u8> {
        f32_bytes(&self.points)
    }
}

/// Flatten a point cloud into the instance data its pipeline reads.
pub fn point_data(mesh: &Mesh) -> PointData {
    let mut points = Vec::with_capacity(mesh.positions.len() * 11);
    for (index, position) in mesh.positions.iter().enumerate() {
        let normal = point_normal(mesh, index);
        let color = base_color(mesh, index);
        // The same read the CPU rasteriser makes when it colours this point, so
        // the two cannot disagree about which channel value belongs to which
        // point. A channel the file has no room for arrives as zero, which is
        // inert: the viewport will not offer a field it cannot fill.
        let sample = Sample::of(mesh, index, normal);
        points.extend_from_slice(&[
            position[0],
            position[1],
            position[2],
            normal[0],
            normal[1],
            normal[2],
            color[0],
            color[1],
            color[2],
            sample.intensity,
            sample.class,
        ]);
    }
    PointData {
        count: mesh.positions.len() as u32,
        points,
    }
}

/// Distance at which a model of unit radius exactly fills the viewport.
fn fit_distance() -> f32 {
    FIT_MARGIN / (FOV_DEG.to_radians() * 0.5).sin()
}

/// Blender workbench's `wrapped_lighting`: a diffuse term whose terminator the
/// light's `wrap` softens. The dot is deliberately unclamped — the wrap is
/// what folds light around past the horizon.
fn wrapped_light(nl: f32, wrap: f32) -> f32 {
    let denom = (wrap + 1.0) * (wrap + 1.0);
    ((nl + wrap) / denom).clamp(0.0, 1.0)
}

/// The specular colour a channel starts from: the dielectric constant for a
/// non-metal, the base colour for a metal, blended by the material's
/// metallic — `mix(vec3(0.05), base_color, metallic)` in the workbench
/// shader.
fn mix_dielectric(albedo: f32, metallic: f32) -> f32 {
    DIELECTRIC_F0 * (1.0 - metallic) + albedo * metallic
}

/// The diffuse half of the studio lighting, summed over the four lights: a
/// colour-free light sum a surface colour multiplies. Mirrors
/// `get_world_lighting`'s diffuse loop in Blender's workbench.
fn studio_diffuse(normal: [f32; 3], lights: &[ModelLight; 4]) -> [f32; 3] {
    let (rest, key) = studio_diffuse_parts(normal, lights);
    [rest[0] + key[0], rest[1] + key[1], rest[2] + key[2]]
}

/// [`studio_diffuse`], split by who casts the light: the three fills (which
/// never shadow) and the key light (which does). Splitting here is what lets
/// a pixel scale *only* the key light's share by its shadow factor — the
/// shader's `select` in its own light loop is the same algebra.
fn studio_diffuse_parts(normal: [f32; 3], lights: &[ModelLight; 4]) -> ([f32; 3], [f32; 3]) {
    let mut rest = [0.0f32; 3];
    let mut key = [0.0f32; 3];
    for (index, light) in lights.iter().enumerate() {
        let lit = wrapped_light(dot(light.direction, normal), light.wrap);
        let target = if index == KEY_LIGHT {
            &mut key
        } else {
            &mut rest
        };
        for (channel, value) in target.iter_mut().zip(light.diffuse) {
            *channel += lit * value;
        }
    }
    (rest, key)
}

/// The environment the specular reflects.
///
/// Blender's material preview shades a metal with a studio HDRI — without
/// something to reflect, a metal's zero diffuse leaves it black. There is no
/// map to sample here, so the studio is written as a function: a soft room
/// gradient, bright overhead and dim below, plus the four rig lights as
/// broadened panels. Roughness widens each panel and fades it toward the
/// room's level — the prefilter's energy-preserving shape, analytic.
fn studio_environment(dir: [f32; 3], lights: &[ModelLight; 4], roughness: f32) -> [f32; 3] {
    let height = (dir[1] * 0.5 + 0.5).clamp(0.0, 1.0);
    let room = 0.05 + 0.30 * height * height;
    let mut sum = [room; 3];
    let shininess = (9.0 * (1.0 - roughness) + 1.0).exp2();
    let focus = 1.0 - 0.7 * roughness;
    for light in lights {
        let lobe = dot(light.direction, dir).clamp(0.0, 1.0).powf(shininess) * focus;
        for (channel, value) in sum.iter_mut().zip(light.specular) {
            *channel += lobe * value * 2.0;
        }
    }
    sum
}

/// The specular half, per surface point: the normalized-Blinn term each light
/// contributes, plus the studio environment the reflection direction sees.
/// The sum is split the way [`studio_diffuse_parts`] is — the key light's
/// share separate from the fills and the environment — because a highlight
/// the key light casts has no business surviving inside that light's shadow.
/// Returns the coloured rest, the coloured key share, and the energy the
/// specular takes out of the diffuse — Blender's single-knob conservation
/// between the two halves of one answer.
///
/// The two halves wear different colours. The direct lights take the
/// Fresnel-lifted specular colour, Blender's fast path. The environment takes
/// the split-sum weighting instead — `F0 · scale + bias` off the baked BRDF
/// table, EEVEE's lightprobe eval — because a surface's grazing response is
/// exactly what the direct lights' one-lift approximation loses, and it is
/// the environment a grazing angle reflects.
fn studio_specular_parts(
    normal: [f32; 3],
    to_eye: [f32; 3],
    lights: &[ModelLight; 4],
    roughness: f32,
    metallic: f32,
    albedo: [f32; 3],
) -> ([f32; 3], [f32; 3], f32) {
    // The Fresnel approximation Blender's fast path uses: the specular colour
    // lifts toward white at grazing angles, the less the rougher the surface.
    let nv = dot(normal, to_eye).clamp(0.0, 1.0);
    let fresnel = (-8.35 * nv).exp2() * (1.0 - roughness);
    let spec_f0 = [
        mix_dielectric(albedo[0], metallic),
        mix_dielectric(albedo[1], metallic),
        mix_dielectric(albedo[2], metallic),
    ];
    let spec_color = [
        spec_f0[0] * (1.0 - fresnel) + fresnel,
        spec_f0[1] * (1.0 - fresnel) + fresnel,
        spec_f0[2] * (1.0 - fresnel) + fresnel,
    ];
    let energy = (spec_color[0] + spec_color[1] + spec_color[2]) / 3.0;
    // The mirror direction the environment term reads: where a perfect
    // reflector would send the view ray.
    let mirror = sub(scale(normal, 2.0 * dot(normal, to_eye)), to_eye);
    let mut rest = [0.0f32; 3];
    let mut key = [0.0f32; 3];
    for (index, light) in lights.iter().enumerate() {
        let half = normalize(add(light.direction, to_eye));
        let spec_angle = dot(half, normal).clamp(0.0, 1.0);
        let nl = dot(light.direction, normal).clamp(0.0, 1.0);
        // A wrapped light is a bigger, softer light: its gloss shrinks and its
        // highlight widens accordingly.
        let gloss = (1.0 - roughness) * (1.0 - light.wrap);
        let shininess = (10.0 * gloss + 1.0).exp2();
        let s = spec_angle.powf(shininess) * nl * (shininess * 0.125 + 1.0);
        let target = if index == KEY_LIGHT {
            &mut key
        } else {
            &mut rest
        };
        for (channel, value) in target.iter_mut().zip(light.specular) {
            *channel += s * value;
        }
    }
    // The direct lights wear the lifted colour...
    for (channel, value) in rest.iter_mut().zip(spec_color) {
        *channel *= value;
    }
    for (channel, value) in key.iter_mut().zip(spec_color) {
        *channel *= value;
    }
    // ...and the environment the split-sum pair: what the surface reflects
    // beyond the four panels themselves, weighted by the integrated Fresnel
    // and geometry response at this incidence and roughness. It is the room's
    // light, not the key's, so it rides the unshadowed share.
    let brdf = environment_brdf_lut().sample(nv, roughness);
    let environment = studio_environment(mirror, lights, roughness);
    for channel in 0..3 {
        let env_color = spec_f0[channel] * brdf[0] + brdf[1];
        rest[channel] += environment[channel] * env_color;
    }
    (rest, key, energy)
}

/// Rasterise into the scratch buffers, background included.
///
/// The buffers are resized in place: a frame that matches the last one pays
/// for clearing, not for allocating.
#[allow(clippy::too_many_arguments)]
fn paint(
    mesh: &Mesh,
    camera: &Camera,
    width: u32,
    height: u32,
    point_radius: f32,
    quality: f32,
    options: RenderOptions,
    scratch: &mut Scratch,
) {
    let w = width as usize;
    let h = height as usize;
    let Scratch {
        colors,
        depth,
        model,
        view,
        shadow: shadow_map,
        ..
    } = scratch;
    colors.clear();
    colors.extend((0..h).flat_map(|y| (0..w).map(move |x| background(x, y, w, h))));
    let longest = mesh.bounds.longest_edge();
    let drawable = longest.is_finite() && (longest > 0.0 || mesh.is_point_cloud());
    if !drawable {
        return;
    }

    // Normalise into a unit bounding sphere so the camera maths never depends
    // on the file's real units.
    let framing = camera.framing(mesh.bounds, w as f32 / h as f32);
    depth.clear();
    depth.resize(w * h, 0.0);

    if mesh.is_point_cloud() {
        let mut target = Target {
            colors,
            depth,
            width: w,
            height: h,
            framing,
            texture: None,
            shadow: None,
        };
        paint_points(&mut target, mesh, point_radius, quality, options.height);
        return;
    }
    if mesh.triangles.is_empty() {
        return;
    }

    // The key light's shadow map, baked before anything shades: every pixel
    // of the frame is about to ask it how much of the key light survives.
    // Off, the map stays empty — and an empty map is "nothing in front of
    // you", which every pixel tests out of fully lit, the same answer the GPU
    // gives when its shadow pass is skipped. The quality step applies here
    // too: a draft's shadow comes from the draft's geometry, the same trade
    // the draft's picture makes.
    let shadow_framing = shadow_framing(&framing);
    shadow_map.clear();
    shadow_map.resize((CPU_SHADOW_MAP_SIZE * CPU_SHADOW_MAP_SIZE) as usize, 0.0);
    let shadow = options.shadows.then(|| {
        rasterize_shadow_map(
            mesh,
            &framing,
            &shadow_framing,
            shadow_map,
            CPU_SHADOW_MAP_SIZE,
            (1.0f32 / quality).round().max(1.0) as usize,
        );
        ShadowMap {
            depth: shadow_map,
            size: CPU_SHADOW_MAP_SIZE,
        }
    });

    // Model-space positions drive the shading, view-space positions the
    // rasterizer; both come out of a single pass over the vertices.
    model.clear();
    view.clear();
    model.reserve(mesh.positions.len());
    view.reserve(mesh.positions.len());
    for p in &mesh.positions {
        model.push(framing.to_unit(*p));
        view.push(framing.to_view(*p));
    }

    let lights = model_space_lights(&framing);
    let eye = framing.eye;
    let (near, _) = framing.depth_range();
    // Textures paint only while the material switch is on and no field look
    // owns the surface colour: the height look wins over the file's own
    // materials, exactly as it wins over their colours.
    let texture = (options.material_colors && mesh.has_textures())
        .then_some(mesh.texture.as_deref())
        .flatten();
    let vertex_count = mesh.positions.len();
    let gouraud = mesh.has_vertex_normals();

    {
        let mut target = Target {
            colors,
            depth,
            width: w,
            height: h,
            framing,
            texture,
            shadow,
        };

        // Quality subsampling: render every `step`th triangle.  At
        // quality 1.0 every triangle draws; at 0.25 only every 4th does.
        let step = (1.0f32 / quality).round().max(1.0) as usize;
        for (tri_idx, triangle) in mesh.triangles.iter().enumerate() {
            if tri_idx % step != 0 {
                continue;
            }
            let (i0, i1, i2) = (
                triangle[0] as usize,
                triangle[1] as usize,
                triangle[2] as usize,
            );
            if i0 >= vertex_count || i1 >= vertex_count || i2 >= vertex_count {
                continue;
            }
            let (p0, p1, p2) = (model[i0], model[i1], model[i2]);
            let face = normalize(cross(sub(p1, p0), sub(p2, p0)));
            if face == [0.0; 3] {
                continue; // Degenerate once normalised.
            }
            let centroid = scale(add(add(p0, p1), p2), 1.0 / 3.0);
            let to_camera = normalize(sub(eye, centroid));
            // Two-sided: an open shell should not have invisible back faces.
            // A closed, outward-wound surface hides its own back faces, so
            // they can be skipped before rasterising instead of shaded and
            // depth-tested: half the triangles, for the same picture. An open
            // shell keeps both faces — its back face is what you see from
            // behind.
            let facing_away = dot(face, to_camera) < 0.0;
            // Culling is the face's own material's business: a double-sided
            // material's back face is the surface seen from behind, so the
            // mesh-level switch only reaches faces whose material lets it.
            // (The GPU, which culls per pipeline rather than per material,
            // reads the same flag per fragment instead — the pictures agree.)
            let material_two_sided = texture
                .and_then(|t| t.double_sided.get(i0).copied())
                .unwrap_or(false);
            if options.cull_backfaces && !material_two_sided && facing_away {
                continue;
            }
            let shaded_face = if facing_away { neg(face) } else { face };

            // One texture per face: a triangle belongs to one primitive, and
            // a primitive carries one material. `NO_TEXTURE` falls out of the
            // slot lookup naturally — no map answers at that index.
            let face_slot = texture
                .and_then(|t| t.slot.get(i0).copied())
                .unwrap_or(NO_TEXTURE);
            // The material's roughness and metallic: sampled from the
            // metallic-roughness texture when the primitive carries one (G =
            // roughness, B = metallic, times the factors), else the
            // fresh-Principled defaults the studio rig assumes. A height look
            // owns the colour but not the finish, so these stand regardless.
            let (roughness, metallic) = match texture.and_then(|t| {
                Some((
                    t.maps.get(*t.mr_slot.get(i0)? as usize)?,
                    *t.factors.get(i0)?,
                    *t.uv.get(i0)?,
                ))
            }) {
                Some((map, factors, uv)) => {
                    let (_r, g, b) = map.sample(uv[0], uv[1]);
                    // The metallic-roughness channels are LINEAR data, not
                    // colour — `sample` decoded them as sRGB on the way out,
                    // so they are re-encoded back to the stored value before
                    // use. A stored 0.5 must read as 0.5, or the finish the
                    // author picked quietly turns to gloss.
                    (
                        encode_channel(g) * factors[1],
                        encode_channel(b) * factors[0],
                    )
                }
                None => (MATERIAL_ROUGHNESS, 0.0),
            };
            let face_albedo = base_color(mesh, i0);
            // The normal map turns the face normal inside the tangent frame
            // the triangle's own UVs describe — per face, like everything
            // else texture-sampled here — and the occlusion map dims the
            // light where the file says its geometry shadows. Faces with
            // neither keep the geometric answer.
            let relief = texture.and_then(|t| {
                let slot = t.normal_slot.get(i0).copied()?;
                if slot == NO_TEXTURE {
                    return None;
                }
                let map = t.maps.get(slot as usize)?;
                let scale = t.normal_scale.get(i0).copied().unwrap_or(1.0);
                relief_normal(
                    [p0, p1, p2],
                    [*t.uv.get(i0)?, *t.uv.get(i1)?, *t.uv.get(i2)?],
                    shaded_face,
                    map,
                    *t.uv.get(i0)?,
                    scale,
                )
            });
            let occlusion = texture
                .and_then(|t| {
                    let slot = t.ao_slot.get(i0).copied()?;
                    if slot == NO_TEXTURE {
                        return None;
                    }
                    let map = t.maps.get(slot as usize)?;
                    let at = *t.uv.get(i0)?;
                    let strength = t.ao_strength.get(i0).copied().unwrap_or(1.0);
                    // The occlusion channel is linear data like the MR pair's:
                    // re-encode the decoded sample back to the stored value.
                    let stored = encode_channel(map.sample(at[0], at[1]).0);
                    Some((1.0 + strength * (stored - 1.0)).clamp(0.0, 1.0))
                })
                .unwrap_or(1.0);
            // The emissive rides to the rasterizer as
            // `[layer, factor r, g, b]`: the layer packs like the others (0 =
            // none, real slots shifted up), and a face with a zero factor
            // gives off nothing however bright its map.
            let emissive = texture
                .map(|t| {
                    let slot = t.emissive_slot.get(i0).copied().unwrap_or(NO_TEXTURE);
                    let factor = t.emissive_factor.get(i0).copied().unwrap_or([0.0; 3]);
                    [
                        if slot == NO_TEXTURE {
                            0.0
                        } else {
                            slot as f32 + 1.0
                        },
                        factor[0],
                        factor[1],
                        factor[2],
                    ]
                })
                .unwrap_or([0.0; 4]);
            // The material's alpha test rides the same per-vertex scalars the
            // other material properties do, read per face like them: the
            // cutoff (negative = opaque) and the base-colour factor's alpha.
            let alpha = texture
                .map(|t| {
                    [
                        t.alpha_cutoff.get(i0).copied().unwrap_or(-1.0),
                        t.alpha_factor.get(i0).copied().unwrap_or(1.0),
                    ]
                })
                .unwrap_or([-1.0, 1.0]);
            // The specular is per face — the same granularity the highlight
            // had before — because interpolating it per vertex would smear it
            // over the whole triangle and lose the very tightness a highlight
            // is. Split by who casts it: the key light's share is what the
            // shadow test gets to dim, the fills and the environment stand
            // whatever the shadow says.
            let (spec_rest, spec_key, spec_energy) = studio_specular_parts(
                relief.unwrap_or(shaded_face),
                to_camera,
                &lights,
                roughness,
                metallic,
                face_albedo,
            );
            // The shadow test's one correction, per face: the slope-scaled
            // depth bias. The more the surface turns away from the key light,
            // the more its interpolated depth can slip behind the slope it
            // lands on. The same correction the shader applies per pixel.
            let light_normal = relief.unwrap_or(shaded_face);
            let no_l = dot(lights[KEY_LIGHT].direction, light_normal).clamp(0.0, 1.0);
            let bias = shadow_bias(no_l, CPU_SHADOW_MAP_SIZE);
            let face_material = FaceShading {
                spec_rest,
                spec_key,
                slot: face_slot,
                emissive,
                alpha,
                bias,
            };
            let mut corners = [Vertex::default(); 3];
            for (slot, index) in [i0, i1, i2].into_iter().enumerate() {
                let geometric = if gouraud {
                    let n = normalize(mesh.normals[index]);
                    if n == [0.0; 3] { face } else { n }
                } else {
                    face
                };
                // The lighting normal is the geometric one flipped to face the
                // camera, so an open shell never shows a black back face —
                // unless a normal map owns the face, which shades flat with
                // its relief normal: per-vertex smoothing would wash the
                // detail the map carries straight out.
                let normal = match relief {
                    Some(relieved) => relieved,
                    None if dot(geometric, to_camera) < 0.0 => neg(geometric),
                    None => geometric,
                };
                // Field colouring is decided per vertex, so a band boundary
                // lands on the geometry rather than on a pixel; the shader does
                // the same from the same model-space position and normal. With
                // no field active the vertex keeps the colour the file gave it
                // — its material — or the flat one when the file had none or
                // the material switch is off.
                let c = options
                    .height
                    .tint_at(&Sample::of(mesh, index, geometric))
                    .unwrap_or(if options.material_colors {
                        base_color(mesh, index)
                    } else {
                        MATERIAL
                    });
                // The light sum rides the vertices (Gouraud), and hands the
                // specular's share of the energy back to the diffuse here —
                // per-face constants, so they scale the corners for free. The
                // occlusion dims the whole light sum: the rig has no separate
                // ambient term for the glTF semantics to single out. The key
                // light's share travels beside the fills' so the shadow test
                // can dim it alone, per pixel.
                let (fills, key) = studio_diffuse_parts(normal, &lights);
                let share = (1.0 - spec_energy) * (1.0 - metallic) * occlusion;
                let mut i = fills;
                let mut key_i = key;
                for channel in i.iter_mut() {
                    *channel *= share;
                }
                for channel in key_i.iter_mut() {
                    *channel *= share;
                }
                let uv = texture
                    .filter(|_| face_slot != NO_TEXTURE)
                    .and_then(|t| t.uv.get(index).copied())
                    .unwrap_or([0.0, 0.0]);
                // Where this corner lands on the shadow map. Zero when there
                // is no map: the pixel never reads it.
                let shadow_at = shadow
                    .as_ref()
                    .map(|_| shadow_framing.uv_depth(model[index]))
                    .unwrap_or([0.0; 3]);
                corners[slot] = Vertex {
                    p: view[index],
                    i,
                    key: key_i,
                    c,
                    uv,
                    shadow: shadow_at,
                };
            }

            let mut polygon = [Vertex::default(); 4];
            let count = clip_near(corners, near, &mut polygon);
            for k in 1..count.saturating_sub(1) {
                target.triangle(polygon[0], polygon[k], polygon[k + 1], face_material);
            }
        }
    }
}

/// The depth bias one shadow test carries, slope-scaled by the surface's
/// incidence on the key light.
///
/// A surface tilted `θ` from the light's view plane sinks `tan θ` of depth
/// per texel of the map (`0.625 / N` is one texel's share of the reversed
/// range), and the PCF reaches `√2` texels diagonally, so the bias is two
/// texels of slope — enough for the taps, small enough that the shadow stays
/// attached. Grazing surfaces would need an unbounded `tan θ`; past 3.0 the
/// bias stops growing and accepts the leeway, which is where peter-panning
/// is least visible anyway. The GPU shader runs the same formula on its own
/// map size.
fn shadow_bias(no_l: f32, map_size: u32) -> f32 {
    let slope = (1.0 - no_l * no_l).max(0.0).sqrt() / no_l.max(0.1);
    (0.625 / map_size as f32) * 2.0 * slope.min(3.0)
}

/// Rasterise the mesh into the key light's depth map, in place.
///
/// Depth-only, reversed-Z, nearest to the light kept — the largest value,
/// which is what a receiver's `reference >= stored` test asks for. There is
/// no near-plane clipping: the orthographic window was sized to hold the
/// whole model, so a triangle can only fall off the map's edge, where its
/// bounding box empties. The winding is normalised per triangle rather than
/// trusted — an open shell shows the light both faces.
fn rasterize_shadow_map(
    mesh: &Mesh,
    framing: &Framing,
    shadow: &ShadowFraming,
    out: &mut [f32],
    size: u32,
    step: usize,
) {
    let n = size as usize;
    let size_f = size as f32;
    for (tri_idx, triangle) in mesh.triangles.iter().enumerate() {
        if tri_idx % step != 0 {
            continue;
        }
        let mut corners = [[0.0f32; 3]; 3];
        let mut drawable = true;
        for (slot, &index) in triangle.iter().enumerate() {
            match mesh.positions.get(index as usize) {
                Some(p) => corners[slot] = shadow.uv_depth(framing.to_unit(*p)),
                None => {
                    drawable = false;
                    break;
                }
            }
        }
        if !drawable {
            continue;
        }
        let area = edge(
            corners[0][0],
            corners[0][1],
            corners[1][0],
            corners[1][1],
            corners[2][0],
            corners[2][1],
        );
        if area.abs() < 1e-9 {
            continue; // Zero light-space area: shades nothing.
        }
        // Fill with the winding normalised, so one inside test serves both
        // faces of a two-sided shell.
        let [c0, c1, c2] = if area > 0.0 {
            [corners[0], corners[1], corners[2]]
        } else {
            [corners[0], corners[2], corners[1]]
        };
        let inv_area = 1.0 / area.abs();
        let min_x = (c0[0].min(c1[0]).min(c2[0]) * size_f).floor().max(0.0) as usize;
        let max_x = (c0[0].max(c1[0]).max(c2[0]) * size_f)
            .ceil()
            .min(n as f32 - 1.0) as usize;
        let min_y = (c0[1].min(c1[1]).min(c2[1]) * size_f).floor().max(0.0) as usize;
        let max_y = (c0[1].max(c1[1]).max(c2[1]) * size_f)
            .ceil()
            .min(n as f32 - 1.0) as usize;
        for py in min_y..=max_y {
            let sample_y = (py as f32 + 0.5) / size_f;
            for px in min_x..=max_x {
                let sample_x = (px as f32 + 0.5) / size_f;
                let w0 = edge(c1[0], c1[1], c2[0], c2[1], sample_x, sample_y) * inv_area;
                if w0 < 0.0 {
                    continue;
                }
                let w1 = edge(c2[0], c2[1], c0[0], c0[1], sample_x, sample_y) * inv_area;
                if w1 < 0.0 {
                    continue;
                }
                let w2 = edge(c0[0], c0[1], c1[0], c1[1], sample_x, sample_y) * inv_area;
                if w2 < 0.0 {
                    continue;
                }
                // Reversed-Z: nearest to the light is the largest depth, and
                // what a receiver's test has to see.
                let depth = w0 * c0[2] + w1 * c1[2] + w2 * c2[2];
                let at = py * n + px;
                if depth > out[at] {
                    out[at] = depth;
                }
            }
        }
    }
}

/// The colour to paint one vertex with: the file's own when it carries one,
/// otherwise the material.
///
/// Shared by the CPU splat and the GPU instance buffer, so a cloud's colours
/// cannot drift between the two renderers.
pub fn base_color(mesh: &Mesh, index: usize) -> [f32; 3] {
    mesh.colors.get(index).copied().unwrap_or(MATERIAL)
}

/// The face normal one normal-map sample rotates.
///
/// The tangent frame comes from the triangle's own edges and UV deltas: the
/// tangent runs along increasing U and the bitangent along increasing V,
/// which is the frame the glTF `TANGENT` attribute describes (checked vertex
/// for vertex against a real export — its tangent agrees with the U
/// direction and its handedness picks the V direction for the bitangent), so
/// the map's red multiplies U's direction and its green V's, no flip. Both
/// directions are edge-vector combinations, in the triangle's plane by
/// construction, so the frame stands orthogonal to `face` already — and
/// `face` is the flipped, camera-facing one, so a back face's relief flips
/// with it. `None` when the UVs are degenerate: the geometric normal is the
/// honest answer there.
fn relief_normal(
    corners: [[f32; 3]; 3],
    uvs: [[f32; 2]; 3],
    face: [f32; 3],
    map: &TextureMap,
    at: [f32; 2],
    scale: f32,
) -> Option<[f32; 3]> {
    let duv1 = [uvs[1][0] - uvs[0][0], uvs[1][1] - uvs[0][1]];
    let duv2 = [uvs[2][0] - uvs[0][0], uvs[2][1] - uvs[0][1]];
    let det = duv1[0] * duv2[1] - duv1[1] * duv2[0];
    if det.abs() < 1e-10 {
        return None;
    }
    let e1 = sub(corners[1], corners[0]);
    let e2 = sub(corners[2], corners[0]);
    let tangent = normalize([
        (e1[0] * duv2[1] - e2[0] * duv1[1]) / det,
        (e1[1] * duv2[1] - e2[1] * duv1[1]) / det,
        (e1[2] * duv2[1] - e2[2] * duv1[1]) / det,
    ]);
    let bitangent = normalize([
        (e2[0] * duv1[0] - e1[0] * duv2[0]) / det,
        (e2[1] * duv1[0] - e1[1] * duv2[0]) / det,
        (e2[2] * duv1[0] - e1[2] * duv2[0]) / det,
    ]);
    // The map's channels are linear data like the metallic-roughness pair's:
    // `sample` decoded them as sRGB on the way out, so they are re-encoded
    // back to the stored value and folded to the -1..1 vector they name. The
    // scale rides the tangent plane; the normal component is what the map
    // says outright.
    let (r, g, b) = map.sample(at[0], at[1]);
    let x = (encode_channel(r) * 2.0 - 1.0) * scale;
    let y = (encode_channel(g) * 2.0 - 1.0) * scale;
    let z = encode_channel(b) * 2.0 - 1.0;
    let relieved = normalize([
        tangent[0] * x + bitangent[0] * y + face[0] * z,
        tangent[1] * x + bitangent[1] * y + face[1] * z,
        tangent[2] * x + bitangent[2] * y + face[2] * z,
    ]);
    (relieved != [0.0; 3]).then_some(relieved)
}

/// Draw a point cloud: one sprite per vertex, shaded and depth-tested.
///
/// Mirrors `fs_point` in `gpu3d.wgsl` — same normal choice, same lighting
/// formula, same sprite size — so a cloud's thumbnail and its viewport frame
/// agree. Everything happens in model space, because that is where the
/// normals and the eye the uniform block carries both live.
fn paint_points(
    target: &mut Target<'_>,
    mesh: &Mesh,
    radius: f32,
    quality: f32,
    height: HeightField,
) {
    let (near, _) = target.framing.depth_range();
    let lights = model_space_lights(&target.framing);
    let eye = target.framing.eye_in_model_space();
    let step = (1.0f32 / quality).round().max(1.0) as usize;

    for (index, position) in mesh.positions.iter().enumerate() {
        if index % step != 0 {
            continue;
        }
        let view = target.framing.to_view(*position);
        if view[2] <= near {
            continue; // Behind the eye, or inside the near plane.
        }
        // The geometry's own normal, which is what the instance buffer hands
        // the GPU and what a dip direction has to be read off — before the flip
        // below turns it to face the camera.
        let geometric = point_normal(mesh, index);
        let to_eye = normalize(sub(eye, *position));
        // Two-sided, exactly as the triangle path is.
        let normal = if dot(geometric, to_eye) < 0.0 {
            neg(geometric)
        } else {
            geometric
        };
        // The same studio rig the triangles shade with, diffuse only: a disc
        // a few pixels across has no room for a highlight to live in.
        let light_sum = studio_diffuse(normal, &lights);
        // Field colouring wins over the file's own colours; that is the whole
        // point of switching it on.
        let base = height
            .tint_at(&Sample::of(mesh, index, geometric))
            .unwrap_or_else(|| base_color(mesh, index));
        let rgb = [
            base[0] * light_sum[0],
            base[1] * light_sum[1],
            base[2] * light_sum[2],
        ];
        target.point(view, radius, rgb);
    }
}

/// The normal to light a cloud point with, in model space.
///
/// A point has no surface, so a file that carries no normals gets the
/// direction it sits in relative to the model centre: the cloud then reads as
/// a lit volume instead of a flat silhouette. A point exactly at the centre
/// has no such direction, so it is lit head-on.
pub fn point_normal(mesh: &Mesh, index: usize) -> [f32; 3] {
    const FALLBACK: [f32; 3] = [0.0, 0.0, 1.0];
    if mesh.has_vertex_normals() {
        let n = normalize(mesh.normals[index]);
        return if n == [0.0; 3] { FALLBACK } else { n };
    }
    let Some(position) = mesh.positions.get(index) else {
        return FALLBACK;
    };
    let direction = normalize(sub(*position, mesh.bounds.center()));
    if direction == [0.0; 3] {
        FALLBACK
    } else {
        direction
    }
}

/// One rasterization target: the colour and depth buffers plus the projection.
struct Target<'a> {
    colors: &'a mut [[f32; 3]],
    /// `1/z` per pixel; zero means "nothing drawn yet", larger is nearer.
    depth: &'a mut [f32],
    width: usize,
    height: usize,
    framing: Framing,
    /// The mesh's texture set, when the material switch serves it.
    texture: Option<&'a TextureData>,
    /// The key light's depth map, when the frame casts shadows.
    shadow: Option<ShadowMap<'a>>,
}

/// A baked shadow map the triangle path samples: the reversed-Z depths and
/// the edge they were rasterised at.
#[derive(Clone, Copy)]
struct ShadowMap<'a> {
    depth: &'a [f32],
    size: u32,
}

impl Target<'_> {
    /// How much of the key light survives at a pixel: 1 fully lit, 0 fully
    /// shadowed, in between on the penumbra the 3×3 taps straddle.
    ///
    /// `s` is the perspective-correct light-space `[u, v, depth]`, `bias` the
    /// face's slope-scaled depth correction. Reversed-Z throughout: a caster
    /// in front stores a *larger* depth, so a reference that fails
    /// `reference >= stored` is standing behind something.
    fn shadow_factor(&self, s: [f32; 3], bias: f32) -> f32 {
        let Some(map) = &self.shadow else {
            return 1.0;
        };
        let size = map.size as i32;
        let x = (s[0] * map.size as f32) as i32;
        let y = (s[1] * map.size as f32) as i32;
        let reference = s[2] + bias;
        let mut sum = 0.0;
        for dy in -1..=1 {
            for dx in -1..=1 {
                let tx = (x + dx).clamp(0, size - 1) as usize;
                let ty = (y + dy).clamp(0, size - 1) as usize;
                if reference >= map.depth[ty * map.size as usize + tx] {
                    sum += 1.0;
                }
            }
        }
        sum / 9.0
    }

    /// Perspective-project one view-space vertex onto the pixel grid.
    fn project(&self, v: Vertex) -> ScreenVertex {
        let (pos, inv_z) = self
            .framing
            .to_screen(v.p, self.width as f32, self.height as f32);
        ScreenVertex {
            x: pos[0],
            y: pos[1],
            inv_z,
            i: v.i,
            key: v.key,
            c: v.c,
            uv: v.uv,
            shadow: v.shadow,
        }
    }

    /// One point sprite: a disc of `radius` pixels around the projected
    /// vertex, depth-tested per pixel. The whole disc shares the point's
    /// depth, which is what makes a dense cloud's surfaces come out smooth
    /// instead of speckled.
    fn point(&mut self, view: [f32; 3], radius: f32, rgb: [f32; 3]) {
        let (pos, inv_z) = self
            .framing
            .to_screen(view, self.width as f32, self.height as f32);
        if !inv_z.is_finite() || inv_z <= 0.0 {
            return; // At or behind the eye: `to_screen` would mirror it.
        }
        let (px_center, py_center) = (pos[0], pos[1]);
        let radius = radius.max(0.5);
        let min_x = (px_center - radius).floor().max(0.0) as usize;
        let max_x = (px_center + radius).ceil().min(self.width as f32 - 1.0) as usize;
        let min_y = (py_center - radius).floor().max(0.0) as usize;
        let max_y = (py_center + radius).ceil().min(self.height as f32 - 1.0) as usize;
        if min_x > max_x || min_y > max_y {
            return; // Fully off screen.
        }
        let radius_squared = radius * radius;

        for py in min_y..=max_y {
            let dy = py as f32 + 0.5 - py_center;
            for px in min_x..=max_x {
                let dx = px as f32 + 0.5 - px_center;
                if dx * dx + dy * dy > radius_squared {
                    continue; // Outside the disc.
                }
                let index = py * self.width + px;
                if inv_z <= self.depth[index] {
                    continue; // Hidden behind whatever is already there.
                }
                self.depth[index] = inv_z;
                // The sprite's shading is scene-linear, like a triangle's: the
                // display transform lands on the written pixel here too.
                self.colors[index] = display_color(rgb);
            }
        }
    }

    /// Rasterise one projected triangle with an edge-function test.
    ///
    /// `material` carries the per-face constants the pixel loop folds in: the
    /// two specular sums (fills+environment, and the key light's own), the
    /// texture slot, the emissive and alpha tests, and the shadow bias.
    fn triangle(&mut self, a: Vertex, b: Vertex, c: Vertex, material: FaceShading) {
        // The face's texture, if the model carries one at this slot. The
        // no-texture marker is `u16::MAX`, which no map answers — the lookup
        // falls out of range and the vertex colour stands alone.
        let slot_texture = self
            .texture
            .as_ref()
            .and_then(|t| t.maps.get(material.slot as usize));
        // The emissive map, when the face carries one — layer `0` means the
        // factor lights the face alone, sampled white needs no map.
        let emissive_map = if material.emissive[0] > 0.0 {
            self.texture
                .as_ref()
                .and_then(|t| t.maps.get(material.emissive[0] as usize - 1))
        } else {
            None
        };
        let (a, b, c) = (self.project(a), self.project(b), self.project(c));
        let area = edge(a.x, a.y, b.x, b.y, c.x, c.y);
        if area.abs() < 1e-9 {
            return; // Zero screen area: nothing to fill.
        }
        let inv_area = 1.0 / area;

        let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as usize;
        let max_x = a.x.max(b.x).max(c.x).ceil().min(self.width as f32 - 1.0) as usize;
        let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as usize;
        let max_y = a.y.max(b.y).max(c.y).ceil().min(self.height as f32 - 1.0) as usize;
        if min_x > max_x || min_y > max_y {
            return; // Fully off screen.
        }

        for py in min_y..=max_y {
            let sample_y = py as f32 + 0.5;
            for px in min_x..=max_x {
                let sample_x = px as f32 + 0.5;
                // Barycentric weights. Normalising by `area` makes the inside
                // test independent of the triangle's winding.
                let w0 = edge(b.x, b.y, c.x, c.y, sample_x, sample_y) * inv_area;
                if w0 < 0.0 {
                    continue;
                }
                let w1 = edge(c.x, c.y, a.x, a.y, sample_x, sample_y) * inv_area;
                if w1 < 0.0 {
                    continue;
                }
                let w2 = edge(a.x, a.y, b.x, b.y, sample_x, sample_y) * inv_area;
                if w2 < 0.0 {
                    continue;
                }
                let inv_z = w0 * a.inv_z + w1 * b.inv_z + w2 * c.inv_z;
                let index = py * self.width + px;
                if inv_z <= self.depth[index] {
                    continue; // Hidden behind whatever is already there.
                }
                self.depth[index] = inv_z;
                // The key light's share rides its own interpolation, scaled by
                // this pixel's shadow factor: a pixel the light cannot see
                // keeps only the fills and the environment. The shadow
                // coordinate is perspective-correct like the UVs — it feeds a
                // projection, not a radiance.
                let shadow = self.shadow_factor(
                    [
                        (w0 * a.shadow[0] * a.inv_z
                            + w1 * b.shadow[0] * b.inv_z
                            + w2 * c.shadow[0] * c.inv_z)
                            / inv_z,
                        (w0 * a.shadow[1] * a.inv_z
                            + w1 * b.shadow[1] * b.inv_z
                            + w2 * c.shadow[1] * c.inv_z)
                            / inv_z,
                        (w0 * a.shadow[2] * a.inv_z
                            + w1 * b.shadow[2] * b.inv_z
                            + w2 * c.shadow[2] * c.inv_z)
                            / inv_z,
                    ],
                    material.bias,
                );
                let key = [
                    w0 * a.key[0] + w1 * b.key[0] + w2 * c.key[0],
                    w0 * a.key[1] + w1 * b.key[1] + w2 * c.key[1],
                    w0 * a.key[2] + w1 * b.key[2] + w2 * c.key[2],
                ];
                let intensity = [
                    w0 * a.i[0] + w1 * b.i[0] + w2 * c.i[0] + shadow * key[0],
                    w0 * a.i[1] + w1 * b.i[1] + w2 * c.i[1] + shadow * key[1],
                    w0 * a.i[2] + w1 * b.i[2] + w2 * c.i[2] + shadow * key[2],
                ];
                // The texture coordinate is perspective-correct: a plain
                // screen-space average of the corners' UVs would swim across
                // the surface as the triangle leans away from the eye.
                let (mut sr, mut sg, mut sb) = (
                    w0 * a.c[0] + w1 * b.c[0] + w2 * c.c[0],
                    w0 * a.c[1] + w1 * b.c[1] + w2 * c.c[1],
                    w0 * a.c[2] + w1 * b.c[2] + w2 * c.c[2],
                );
                let mut lit = [0.0; 3];
                if slot_texture.is_some() || emissive_map.is_some() {
                    let u =
                        (w0 * a.uv[0] * a.inv_z + w1 * b.uv[0] * b.inv_z + w2 * c.uv[0] * c.inv_z)
                            / inv_z;
                    let v =
                        (w0 * a.uv[1] * a.inv_z + w1 * b.uv[1] * b.inv_z + w2 * c.uv[1] * c.inv_z)
                            / inv_z;
                    if let Some(map) = slot_texture {
                        // The alpha test comes before the colour is read: a
                        // texel the material rejects draws nothing at all,
                        // which is what cuts the holes in a foliage card.
                        if material.alpha[0] >= 0.0
                            && map.sample_alpha(u, v) * material.alpha[1] < material.alpha[0]
                        {
                            continue;
                        }
                        // Perspective-correct UV, then nearest-neighbour
                        // sample.
                        let (tr, tg, tb) = map.sample(u, v);
                        sr *= tr;
                        sg *= tg;
                        sb *= tb;
                    }
                    // The emissive adds, unlit: the map's colour times the
                    // factor, or the factor alone when the primitive carries
                    // no map.
                    match emissive_map {
                        Some(map) => {
                            let (er, eg, eb) = map.sample(u, v);
                            lit = [
                                material.emissive[1] * er,
                                material.emissive[2] * eg,
                                material.emissive[3] * eb,
                            ];
                        }
                        None => lit = material.emissive[1..4].try_into().unwrap_or([0.0; 3]),
                    }
                }
                // The specular reassembles the same way the diffuse does: the
                // key light's share dims with the shadow, the rest stands.
                let spec = [
                    material.spec_rest[0] + shadow * material.spec_key[0],
                    material.spec_rest[1] + shadow * material.spec_key[1],
                    material.spec_rest[2] + shadow * material.spec_key[2],
                ];
                // The shading is scene-linear — the studio lights, the file's
                // colours and the decoded texture all are — so the display
                // transform lands here, on the assembled pixel, where
                // Blender's does.
                self.colors[index] = display_color([
                    sr * intensity[0] + spec[0] + lit[0],
                    sg * intensity[1] + spec[1] + lit[1],
                    sb * intensity[2] + spec[2] + lit[2],
                ]);
            }
        }
    }
}

/// The per-face constants one triangle shades with, passed to
/// [`Target::triangle`] as one record.
#[derive(Clone, Copy)]
struct FaceShading {
    /// The coloured specular sum of the three fills plus the environment —
    /// the share no shadow can dim.
    spec_rest: [f32; 3],
    /// The coloured specular sum of the key light alone, scaled per pixel by
    /// the shadow factor.
    spec_key: [f32; 3],
    /// The face's base-colour texture slot.
    slot: u16,
    /// `[layer, factor r, g, b]`, as [`Target::triangle`] documents.
    emissive: [f32; 4],
    /// `[cutoff, factor]`, the material's alpha test.
    alpha: [f32; 2],
    /// The face's slope-scaled shadow bias.
    bias: f32,
}

/// A view-space vertex; `i` is the fills' light sum, `key` the key light's
/// own share and `c` the base colour, all carried through clipping and
/// interpolated per pixel.
#[derive(Clone, Copy, Default)]
struct Vertex {
    /// x right, y up, z forward.
    p: [f32; 3],
    /// The fills' diffuse sum, with the specular's energy share already taken
    /// out. Three channels because the rig's lights are coloured, if only
    /// slightly.
    i: [f32; 3],
    /// The key light's diffuse share, adjusted the same way — the part the
    /// shadow test dims.
    key: [f32; 3],
    /// Base surface colour: the material or a height band.
    c: [f32; 3],
    /// Texture coordinate, carried for the textured path.
    uv: [f32; 2],
    /// Light-space `[u, v, reversed depth]` on the shadow map.
    shadow: [f32; 3],
}

/// A projected vertex.
#[derive(Clone, Copy)]
struct ScreenVertex {
    x: f32,
    y: f32,
    /// `1/z`, linear in screen space and larger when nearer.
    inv_z: f32,
    i: [f32; 3],
    key: [f32; 3],
    c: [f32; 3],
    uv: [f32; 2],
    shadow: [f32; 3],
}

/// Twice the signed area of the triangle `(a, b, p)`; the sign says which side
/// of the directed edge `a → b` the point `p` lies on.
fn edge(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (bx - ax) * (py - ay) - (by - ay) * (px - ax)
}

/// Clip a triangle against the `z >= near` plane in view space, writing the
/// resulting convex polygon (0, 3 or 4 vertices) into `out`.
fn clip_near(triangle: [Vertex; 3], near: f32, out: &mut [Vertex; 4]) -> usize {
    let mut count = 0;
    for i in 0..3 {
        let current = triangle[i];
        let next = triangle[(i + 1) % 3];
        let current_in = current.p[2] >= near;
        let next_in = next.p[2] >= near;
        if current_in {
            out[count] = current;
            count += 1;
        }
        if current_in != next_in {
            let t = (near - current.p[2]) / (next.p[2] - current.p[2]);
            let lerp = |a: [f32; 3], b: [f32; 3]| {
                [
                    a[0] + (b[0] - a[0]) * t,
                    a[1] + (b[1] - a[1]) * t,
                    a[2] + (b[2] - a[2]) * t,
                ]
            };
            out[count] = Vertex {
                p: lerp3(current.p, next.p, t),
                uv: [
                    current.uv[0] + (next.uv[0] - current.uv[0]) * t,
                    current.uv[1] + (next.uv[1] - current.uv[1]) * t,
                ],
                i: lerp(current.i, next.i),
                key: lerp(current.key, next.key),
                c: lerp(current.c, next.c),
                shadow: lerp(current.shadow, next.shadow),
            };
            count += 1;
        }
    }
    count
}

/// Eye-dome lighting and gap filling for a rendered point cloud, in place.
///
/// Both come from the same idea, and both are what Nimbus's `EDL` and
/// `ComposeImage` compute passes do on the GPU: the frame is not a picture of a
/// surface but a scatter of discs, and the shape inside it only becomes
/// legible once the scatter is closed up and shaded by how the cloud turns away
/// from the eye.
///
/// Pass one builds `-log2(depth)` per pixel. Eye-dome lighting compares the
/// *ratio* of two depths, and taking the logarithm once turns that comparison
/// into a subtraction, which is what makes the second pass cheap enough to run
/// over every pixel.
///
/// Pass two darkens a pixel by how much nearer its four orthogonal neighbours
/// are: `exp(-mean(max(0, l - l_neighbour)) * strength)`. A crease, a
/// silhouette or a surface turning away goes dark; a surface facing the camera
/// stays bright. That is the whole illusion — a cloud rendered flat reads as
/// dust, and the same cloud with eye-dome lighting reads as a surface.
///
/// Pass three fills the pixels nothing drew, from the colours around them, and
/// only where the neighbourhood supports it: filling next to a silhouette
/// would grow the model outwards into the background by a pixel.
fn enhance_points(scratch: &mut Scratch, width: usize, height: usize) {
    /// Radius searched for colour when filling a gap. Nimbus keeps this
    /// adjustable in the UI and defaults to one pixel.
    const FILL_RADIUS: usize = 1;

    let pixels = width * height;
    if pixels == 0 || scratch.colors.len() < pixels || scratch.depth.len() < pixels {
        return;
    }
    let drawn = |depth: &f32| *depth > 0.0;

    // Pass one: the log-depth image, with 0.0 marking "nothing here" — the
    // same sentinel the depth buffer itself uses.
    scratch.log_depth.clear();
    scratch.log_depth.reserve(pixels);
    for depth in &scratch.depth[..pixels] {
        // `depth` is `1/z`, so a larger value is a nearer pixel and
        // `-log2(depth)` grows with distance; subtracting two of them gives
        // the log of the ratio of their distances, in the right order.
        scratch
            .log_depth
            .push(if drawn(depth) { -depth.log2() } else { 0.0 });
    }

    // Pass two: eye-dome lighting, over the pixels that drew something. It
    // only ever darkens, so the background and the unlit pixels are untouched.
    {
        let Scratch {
            colors,
            depth,
            log_depth,
            ..
        } = scratch;
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                if !drawn(&depth[index]) {
                    continue;
                }
                let center = log_depth[index];
                let mut sum = 0.0;
                let mut count = 0.0;
                let mut neighbour = |nx: usize, ny: usize| {
                    let value = log_depth[ny * width + nx];
                    if drawn(&depth[ny * width + nx]) {
                        sum += (center - value).max(0.0);
                        count += 1.0;
                    }
                };
                if x > 0 {
                    neighbour(x - 1, y);
                }
                if x + 1 < width {
                    neighbour(x + 1, y);
                }
                if y > 0 {
                    neighbour(x, y - 1);
                }
                if y + 1 < height {
                    neighbour(x, y + 1);
                }
                let factor = if count == 0.0 {
                    1.0
                } else {
                    (-(sum / count) * EDL_STRENGTH).exp()
                };
                for channel in colors[index].iter_mut() {
                    *channel *= factor;
                }
            }
        }
    }

    // Pass three: fill the gaps the discs left between them. Reading only
    // pixels that drew something, so filling in place cannot feed a filled
    // colour back into the average.
    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            if drawn(&scratch.depth[index]) {
                continue;
            }
            let (x0, x1) = (
                x.saturating_sub(FILL_RADIUS),
                (x + FILL_RADIUS).min(width - 1),
            );
            let (y0, y1) = (
                y.saturating_sub(FILL_RADIUS),
                (y + FILL_RADIUS).min(height - 1),
            );
            // The eight neighbours, in the order Nimbus's shader reads them:
            // lt, mt, rt, lm, rm, lb, mb, rb.
            let occupied = |dx: usize, dy: usize| drawn(&scratch.depth[dy * width + dx]);
            let (has_left, has_right) = (x > x0, x < x1);
            let (has_up, has_down) = (y > y0, y < y1);
            let lt = has_left && has_up && occupied(x - 1, y - 1);
            let mt = has_up && occupied(x, y - 1);
            let rt = has_right && has_up && occupied(x + 1, y - 1);
            let lm = has_left && occupied(x - 1, y);
            let rm = has_right && occupied(x + 1, y);
            let lb = has_left && has_down && occupied(x - 1, y + 1);
            let mb = has_down && occupied(x, y + 1);
            let rb = has_right && has_down && occupied(x + 1, y + 1);

            let neighbours = usize::from(lt)
                + usize::from(mt)
                + usize::from(rt)
                + usize::from(lm)
                + usize::from(rm)
                + usize::from(lb)
                + usize::from(mb)
                + usize::from(rb);
            let window = (x1 - x0 + 1) * (y1 - y0 + 1) - 1;
            // Nimbus's rule, kept as it is: a gap that is completely surrounded
            // is filled, and so is one where every one of the eight five-pixel
            // dominoes around it has some support. On a full 3×3 that is
            // `sum == 8`, and the domino test is what lets a slightly ragged
            // hole close too — without it, interior gaps that are missing one
            // neighbour would stay black.
            let dominoes = [
                [lt, mt, rt, rm, mb],
                [lt, mt, rt, lm, rm],
                [lt, mt, lm, lb, mb],
                [lm, rm, lb, mb, rb],
                [lt, mt, rt, rm, rb],
                [lt, mt, rt, lm, lb],
                [lt, lm, lb, mb, rb],
                [rt, rm, rb, mb, lb],
            ];
            let surrounded = neighbours == window;
            let every_direction = dominoes.iter().all(|set| set.iter().any(|v| *v));
            if !surrounded && !every_direction {
                continue;
            }

            let mut sum = [0.0f32; 3];
            let mut count = 0.0;
            for ny in y0..=y1 {
                for nx in x0..=x1 {
                    if !occupied(nx, ny) {
                        continue;
                    }
                    let color = scratch.colors[ny * width + nx];
                    for channel in 0..3 {
                        sum[channel] += color[channel];
                    }
                    count += 1.0;
                }
            }
            if count == 0.0 {
                continue;
            }
            scratch.colors[index] = [sum[0] / count, sum[1] / count, sum[2] / count];
        }
    }
}

/// Background gradient with a soft corner vignette.
fn background(x: usize, y: usize, width: usize, height: usize) -> [f32; 3] {
    let vertical = (y as f32 + 0.5) / height as f32;
    let top = lerp3(BG_TOP, BG_BOTTOM, vertical);
    let nx = (x as f32 + 0.5) / width as f32 * 2.0 - 1.0;
    let ny = (y as f32 + 0.5) / height as f32 * 2.0 - 1.0;
    let vignette = ((nx * nx + ny * ny) * 0.5).min(1.0) * VIGNETTE;
    lerp3(top, BG_BOTTOM, vignette)
}

/// The model card's background as tight RGBA bytes for a `width`×`height`
/// image. The other generated cards — an audio waveform, a text/subtitle card
/// — composite their ink over this so they sit on the exact same surface as a
/// model card instead of a paper of their own.
pub fn background_rgba(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let [r, g, b] = background(x, y, w, h);
            let i = (y * w + x) * 4;
            out[i] = (r * 255.0).round().clamp(0.0, 255.0) as u8;
            out[i + 1] = (g * 255.0).round().clamp(0.0, 255.0) as u8;
            out[i + 2] = (b * 255.0).round().clamp(0.0, 255.0) as u8;
            out[i + 3] = 255;
        }
    }
    out
}

/// Box-filter an oversized buffer down to the requested size.
fn downsample(src: &[[f32; 3]], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<[f32; 3]> {
    let sx = (sw / dw).max(1);
    let sy = (sh / dh).max(1);
    let mut out = vec![[0f32; 3]; dw * dh];
    for y in 0..dh {
        for x in 0..dw {
            let mut sum = [0f32; 3];
            let mut count = 0.0f32;
            for dy in 0..sy {
                for dx in 0..sx {
                    let px = (x * sx + dx).min(sw - 1);
                    let py = (y * sy + dy).min(sh - 1);
                    let c = src[py * sw + px];
                    sum[0] += c[0];
                    sum[1] += c[1];
                    sum[2] += c[2];
                    count += 1.0;
                }
            }
            out[y * dw + x] = [sum[0] / count, sum[1] / count, sum[2] / count];
        }
    }
    out
}

/// Quantise the display-space buffer into BGRA bytes, fully opaque.
fn to_bgra(colors: &[[f32; 3]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(colors.len() * 4);
    for c in colors {
        for channel in [2usize, 1, 0] {
            out.push(to_byte(c[channel]));
        }
        out.push(255);
    }
    out
}

/// Clamp and scale one display-space channel to a byte.
fn to_byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// The scene-linear → sRGB display transform, precomputed: entry `i` is the
/// encoded channel value for linear input `i / 4095`.
///
/// Blender shades in scene-linear and converts on the way to the screen — its
/// studio-light numbers are linear intensities, and so are a glTF file's
/// colours. The shading result is therefore encoded where the shading is
/// written, exactly where Blender's display transform sits, which keeps the
/// depth buffer, the eye-dome pass and the box filter working on the same
/// display-space values they always did. 4096 bins hold the quantised output
/// within one LSB of the exact curve, and a table beats a `powf` per channel
/// per pixel in the rasteriser's hot loop.
fn encode_lut() -> &'static [f32; 4096] {
    static LUT: std::sync::OnceLock<[f32; 4096]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut table = [0f32; 4096];
        for (entry, value) in table.iter_mut().enumerate() {
            let x = entry as f32 / 4095.0;
            *value = if x <= 0.0031308 {
                12.92 * x
            } else {
                1.055 * x.powf(1.0 / 2.4) - 0.055
            };
        }
        table
    })
}

/// Encode one scene-linear channel for the display-space buffer.
fn encode_channel(value: f32) -> f32 {
    let lut = encode_lut();
    let index = (value.clamp(0.0, 1.0) * 4095.0).round() as usize;
    lut[index]
}

/// Scene-linear sRGB → display code values: Blender's default AgX view
/// transform, in the compact form three.js fits to Blender's own AgX LUTs —
/// into Rec.2020, the inset matrix, a log2 encode that the contrast spline
/// shapes, and back out — then the sRGB encode the screen wants. This is
/// what makes the highlights roll off and the saturated colours hold their
/// hue the way Blender's viewport shows them; the sRGB-only transform
/// clipped them hard.
///
/// Only the *final* pixel goes through here: the metallic-roughness and
/// normal-map channels are stored data, not display colour, and keep the
/// bare [`encode_channel`].
fn display_color(color: [f32; 3]) -> [f32; 3] {
    // Rows multiply the colour vector: row i of each matrix produces output
    // channel i. The AgX pair comes from three.js's fit (its GLSL matrices
    // are column-major; these are the same numbers, transposed to rows).
    const SRGB_TO_REC2020: [[f32; 3]; 3] = [
        [0.627_403_9, 0.329_283, 0.043_313_1],
        [0.069_097_3, 0.919_540_4, 0.011_362_3],
        [0.016_391_4, 0.088_013_3, 0.895_595_3],
    ];
    const REC2020_TO_SRGB: [[f32; 3]; 3] = [
        [1.660_491, -0.587_641_1, -0.072_849_9],
        [-0.124_550_5, 1.132_899_9, -0.008_349_4],
        [-0.018_150_8, -0.100_578_9, 1.118_729_7],
    ];
    const AGX_INSET: [[f32; 3]; 3] = [
        [0.856_627_2, 0.095_121_2, 0.048_251_6],
        [0.137_319, 0.761_242, 0.101_439],
        [0.111_898_2, 0.076_799_4, 0.811_302_4],
    ];
    const AGX_OUTSET: [[f32; 3]; 3] = [
        [1.127_100_6, -0.110_606_6, -0.016_493_9],
        [-0.141_329_8, 1.157_823_7, -0.016_493_9],
        [-0.141_329_8, -0.110_606_6, 1.251_936_4],
    ];
    // log2(2^-10 * 0.18) .. log2(2^6.5 * 0.18): the scene range AgX was fit
    // to, in stops around middle grey.
    const MIN_EV: f32 = -12.473_93;
    const MAX_EV: f32 = 4.026_069;

    let mul = |m: [[f32; 3]; 3], v: [f32; 3]| {
        [
            m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        ]
    };
    // The contrast spline: a monotone curve through middle grey with the
    // shoulder and toe AgX is named for.
    let contrast = |x: f32| {
        let x2 = x * x;
        let x4 = x2 * x2;
        15.5 * x4 * x2 - 40.14 * x4 * x + 31.96 * x4 - 6.868 * x2 * x + 0.4298 * x2 + 0.1191 * x
            - 0.00232
    };

    let mut c = mul(SRGB_TO_REC2020, color);
    c = mul(AGX_INSET, c);
    let mut c = [
        (c[0].max(1e-10)).log2(),
        (c[1].max(1e-10)).log2(),
        (c[2].max(1e-10)).log2(),
    ];
    for channel in &mut c {
        let t = ((*channel - MIN_EV) / (MAX_EV - MIN_EV)).clamp(0.0, 1.0);
        *channel = contrast(t);
    }
    let c = mul(AGX_OUTSET, c);
    let c = [
        c[0].max(0.0).powf(2.2),
        c[1].max(0.0).powf(2.2),
        c[2].max(0.0).powf(2.2),
    ];
    let c = mul(REC2020_TO_SRGB, c);
    [
        encode_channel(c[0].clamp(0.0, 1.0)),
        encode_channel(c[1].clamp(0.0, 1.0)),
        encode_channel(c[2].clamp(0.0, 1.0)),
    ]
}

/// Fold an angle into `-π..π` so repeated orbiting cannot drift.
fn wrap_angle(angle: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut a = angle % TAU;
    if a > PI {
        a -= TAU;
    } else if a < -PI {
        a += TAU;
    }
    a
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn neg(a: [f32; 3]) -> [f32; 3] {
    [-a[0], -a[1], -a[2]]
}

fn scale(a: [f32; 3], factor: f32) -> [f32; 3] {
    [a[0] * factor, a[1] * factor, a[2] * factor]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Unit-length copy of `a`, or the zero vector when `a` is too short to
/// normalise.
fn normalize(a: [f32; 3]) -> [f32; 3] {
    let length = dot(a, a).sqrt();
    if length > 1e-12 {
        scale(a, 1.0 / length)
    } else {
        [0.0; 3]
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::formats::load_obj;
    use crate::media::height_color::{
        Field, FieldData, HeightLook, HeightMode, Scale, scale_by_id,
    };

    /// A closed cube in the unit range, two triangles per face.
    fn cube() -> Mesh {
        load_obj(
            "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\n\
             v 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
             f 1 2 3\nf 1 3 4\nf 5 6 7\nf 5 7 8\n\
             f 1 2 6\nf 1 6 5\nf 2 3 7\nf 2 7 6\n\
             f 3 4 8\nf 3 8 7\nf 4 1 5\nf 4 5 8\n",
        )
        .expect("cube parses")
    }

    /// A wide quad in the XY plane facing +Z (bright), optionally with a
    /// smaller quad hovering in front of its centre that faces +X — edge-on to
    /// the key light, so it shades at ambient only. The small quad is listed
    /// *first*, so a renderer that let later triangles overwrite earlier ones
    /// (instead of testing depth) would show the bright backdrop.
    fn occluded_quad(with_occluder: bool) -> Mesh {
        // Normal 1: +X, edge-on to the key light, so it shades at ambient only.
        // Normal 2: +Z, facing the light, so it shades bright.
        let mut obj = String::from("vn 1 0 0\nvn 0 0 1\n");
        let mut base = 1u32;
        if with_occluder {
            obj.push_str("v -0.3 -0.3 0.5\nv 0.3 -0.3 0.5\nv 0.3 0.3 0.5\nv -0.3 0.3 0.5\n");
            obj.push_str("f 1//1 2//1 3//1\nf 1//1 3//1 4//1\n");
            base = 5;
        }
        obj.push_str("v -1 -1 0\nv 1 -1 0\nv 1 1 0\nv -1 1 0\n");
        obj.push_str(&format!(
            "f {a}//2 {b}//2 {c}//2\nf {a}//2 {c}//2 {d}//2\n",
            a = base,
            b = base + 1,
            c = base + 2,
            d = base + 3,
        ));
        load_obj(&obj).expect("layered quads parse")
    }

    /// Pixels that are clearly model rather than background.
    fn lit_pixels(frame: &Frame) -> usize {
        frame
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[1] < 220)
            .count()
    }

    /// A mesh that carries colours hands them to the vertex buffer — one RGB
    /// per buffered vertex, expanded per face exactly when the geometry is —
    /// and a mesh without colours leaves the array empty, which is what puts
    /// it on the plain pipeline and its flat material.
    #[test]
    fn vertex_data_carries_colours_exactly_when_the_mesh_has_them() {
        let mut colored = cube();
        colored.colors = vec![[0.2, 0.4, 0.8]; colored.positions.len()];
        for flip in [false, true] {
            let data = vertex_data_with(&colored, flip);
            assert_eq!(data.colors.len(), data.vertex_count as usize * 3);
            assert_eq!(data.colors[0], 0.2);
            assert_eq!(data.colors[1], 0.4);
        }
        let plain = cube();
        for flip in [false, true] {
            assert!(vertex_data_with(&plain, flip).colors.is_empty());
        }
    }

    fn brightness(p: [u8; 4]) -> u32 {
        p[0] as u32 + p[1] as u32 + p[2] as u32
    }

    /// A camera-facing quad wearing one 2×2 map whose first texel is fully
    /// transparent and whose second is opaque red, sampled across the whole
    /// face: the UVs run 0.1..0.7, so both texels are hit. `cutoff` decides
    /// whether the material tests alpha at all.
    fn textured_quad(cutoff: f32) -> Mesh {
        let mut mesh = Mesh::from_parts(
            vec![
                [0.0f32, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
            ],
            vec![[0.0f32, 0.0, 1.0]; 4],
            vec![[1.0f32; 3]; 4],
            vec![[0, 1, 2], [0, 2, 3]],
        )
        .expect("quad builds");
        mesh.texture = Some(Box::new(TextureData {
            uv: vec![[0.1, 0.1], [0.7, 0.1], [0.7, 0.7], [0.1, 0.7]],
            slot: vec![0; 4],
            mr_slot: vec![NO_TEXTURE; 4],
            factors: vec![[0.0, 1.0]; 4],
            normal_slot: vec![NO_TEXTURE; 4],
            normal_scale: vec![1.0; 4],
            ao_slot: vec![NO_TEXTURE; 4],
            ao_strength: vec![1.0; 4],
            emissive_slot: vec![NO_TEXTURE; 4],
            emissive_factor: vec![[0.0; 3]; 4],
            alpha_cutoff: vec![cutoff; 4],
            alpha_factor: vec![1.0; 4],
            double_sided: vec![false; 4],
            maps: vec![TextureMap {
                rgba: vec![
                    0, 0, 0, 0, // transparent: the texel the test rejects
                    255, 0, 0, 255, // opaque red
                    255, 0, 0, 255, 255, 0, 0, 255,
                ],
                width: 2,
                height: 2,
            }],
        }));
        mesh
    }

    /// The alpha test is what cuts the holes in a cutout texture: with the
    /// material masked, the transparent texel's share of the face draws
    /// nothing (background shows through); with the test off, the same face
    /// draws everywhere — strictly more lit pixels.
    #[test]
    fn an_alpha_masked_texture_draws_nothing_where_coverage_fails() {
        let frame = |mesh: &Mesh| render(mesh, &Camera::default(), 160, 120, 1, 1.0);
        let opaque = lit_pixels(&frame(&textured_quad(-1.0)));
        let masked = lit_pixels(&frame(&textured_quad(0.5)));
        assert!(opaque > 0, "the opaque quad draws");
        assert!(
            masked < opaque,
            "the mask must hide part of the face ({masked} lit against {opaque})"
        );
        assert!(masked > 0, "the opaque texel's share still draws");
    }

    /// A wide ground plane at y = 0 carrying a small box at its centre: the
    /// shape the shadow tests need — a caster above a receiver, with the key
    /// light above the horizon, so the box's shadow falls on visible ground.
    pub(super) fn occluder_over_ground() -> Mesh {
        load_obj(
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

    /// The shadow map is a depth render of the scene from the key light:
    /// where the box hangs over the ground its depth — nearer to the light,
    /// so larger in the reversed map — replaces the ground's, and the
    /// ground's own share is drawn everywhere else.
    #[test]
    fn the_shadow_map_stores_the_caster_over_the_receiver() {
        let mesh = occluder_over_ground();
        let camera = Camera::default();
        let framing = camera.framing(mesh.bounds, 1.0);
        let shadow = shadow_framing(&framing);
        let mut map = vec![0.0f32; (CPU_SHADOW_MAP_SIZE * CPU_SHADOW_MAP_SIZE) as usize];
        rasterize_shadow_map(&mesh, &framing, &shadow, &mut map, CPU_SHADOW_MAP_SIZE, 1);
        // The ground alone, for comparison.
        let mut without = vec![0.0f32; map.len()];
        let ground_only = load_obj(
            "vn 0 1 0\n\
             v -5 0 -5\nv 5 0 -5\nv 5 0 5\nv -5 0 5\n\
             f 1//1 3//1 2//1\nf 1//1 4//1 3//1\n",
        )
        .expect("the ground parses");
        rasterize_shadow_map(
            &ground_only,
            &framing,
            &shadow,
            &mut without,
            CPU_SHADOW_MAP_SIZE,
            1,
        );
        // The box's projection must have pushed some texels' depth above the
        // ground's, and the ground's own share must be untouched.
        let raised = map
            .iter()
            .zip(without.iter())
            .filter(|(with, ground)| **with > **ground + 1e-4)
            .count();
        assert!(
            raised > 0,
            "the caster never displaced the receiver's depth"
        );
        assert!(
            map.iter().filter(|d| **d > 0.0).count() > raised,
            "the ground itself was drawn too"
        );
    }

    /// The split-sum table's two anchors: a perfect mirror facing the
    /// environment passes it through at its F0 (scale 1, bias 0), and a
    /// grazing, fully rough surface carries a strong white lift (a bias well
    /// above zero). The pair stays bounded everywhere in between — a runaway
    /// table would blow the environment term past the display transform's
    /// shoulder.
    #[test]
    fn the_split_sum_table_passes_a_mirror_and_lifts_a_grazing_rough_surface() {
        let lut = environment_brdf_lut();
        let [scale, bias] = lut.sample(1.0, 0.0);
        assert!(
            (scale - 1.0).abs() < 0.02,
            "a mirror must read the environment unweighted, got scale {scale}"
        );
        assert!(
            bias.abs() < 0.02,
            "a mirror carries no white lift, got {bias}"
        );
        let [_, bias] = lut.sample(0.05, 1.0);
        assert!(
            bias > 0.05,
            "a grazing rough surface lifts, got bias {bias}"
        );
        for iy in 0..BRDF_LUT_SIZE as usize {
            for ix in 0..BRDF_LUT_SIZE as usize {
                let [s, b] = lut.data[iy * BRDF_LUT_SIZE as usize + ix];
                assert!(
                    (0.0..=4.0).contains(&s) && (0.0..=4.0).contains(&b),
                    "the table left its range at ({ix}, {iy}): {s}, {b}"
                );
            }
        }
    }

    /// A material's doubleSided flag overrides the mesh-level cull: the same
    /// open quad seen from behind vanishes under culling when its material
    /// is single-sided and stays when it is not.
    #[test]
    fn a_double_sided_material_keeps_the_back_face_culling_would_erase() {
        let frame = |doubled: bool, cull: bool| {
            let mut mesh = textured_quad(-1.0);
            if let Some(texture) = mesh.texture.as_mut() {
                for flag in texture.double_sided.iter_mut() {
                    *flag = doubled;
                }
            }
            // The quad faces +Z; the camera is parked half a turn away, so
            // the whole picture is the back face the cull is about.
            let camera = Camera {
                yaw: std::f32::consts::PI,
                ..Camera::default()
            };
            render_with_scratch(
                &mesh,
                &camera,
                96,
                96,
                1,
                1.0,
                RenderOptions {
                    cull_backfaces: cull,
                    ..Default::default()
                },
                &mut Scratch::default(),
            )
        };
        assert!(lit_pixels(&frame(false, false)) > 0, "the back face draws");
        assert_eq!(
            lit_pixels(&frame(false, true)),
            0,
            "culling erased a single-sided back face"
        );
        assert!(
            lit_pixels(&frame(true, true)) > 0,
            "a double-sided material drew past the cull"
        );
    }

    /// The end-to-end effect: the box darkens the ground it hangs over. With
    /// the shadow pass the frame differs from the plain one and its mean
    /// brightness is strictly lower — the shadow scales the key light's
    /// share down from full strength, never up.
    #[test]
    fn an_occluder_darkens_the_ground_on_the_cpu() {
        let mesh = occluder_over_ground();
        let camera = Camera::default();
        let frame = |shadows: bool| {
            render_with_scratch(
                &mesh,
                &camera,
                320,
                240,
                1,
                1.0,
                RenderOptions {
                    shadows,
                    ..Default::default()
                },
                &mut Scratch::default(),
            )
        };
        let lit = frame(true);
        let plain = frame(false);
        assert_ne!(lit.bgra, plain.bgra, "the shadow has to change the frame");
        let mean = |frame: &Frame| {
            frame
                .bgra
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

    /// Renders one frame with the enhancement on, for the tests below.
    fn enhanced(mesh: &Mesh, camera: &Camera, size: u32) -> Frame {
        render_with_scratch(
            mesh,
            camera,
            size,
            size,
            1,
            1.0,
            RenderOptions {
                cull_backfaces: false,
                enhance_points: true,
                material_colors: true,
                shadows: false,
                height: HeightField::default(),
            },
            &mut Scratch::default(),
        )
    }

    /// A cube whose triangles all wind the same way, seen from outside.
    ///
    /// The `cube()` fixture above is the more interesting case: its faces are
    /// wound as a human wrote them, which is inconsistent, so it is reported
    /// as two-sided and never culled. That is exactly what a renderer has to
    /// do with most files in the wild, and it is why culling is opt-in.
    fn closed_cube() -> Mesh {
        let obj = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\n\
                   v 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
                   f 1 3 2\nf 1 4 3\nf 5 6 7\nf 5 7 8\n\
                   f 1 2 6\nf 1 6 5\nf 4 8 7\nf 4 7 3\n\
                   f 1 5 8\nf 1 8 4\nf 2 3 7\nf 2 7 6\n";
        let mesh = load_obj(obj).expect("cube parses");
        assert_eq!(
            mesh.winding(),
            crate::media::formats::types::Winding::ClosedOutward,
            "the fixture must be wound consistently to test culling"
        );
        mesh
    }

    /// A cloud sparse enough to leave gaps between its sprites: the case the
    /// enhancement exists for.
    fn sparse_cloud() -> Mesh {
        let mut points = Vec::new();
        let mut colors = Vec::new();
        for y in 0..12 {
            for x in 0..12 {
                points.push([x as f32 * 0.02 - 0.12, y as f32 * 0.02 - 0.12, 0.0]);
                colors.push([0.6, 0.6, 0.6]);
            }
        }
        crate::media::formats::types::Mesh::finish_points(points, Vec::new(), colors)
            .expect("cloud builds")
    }

    /// Gap filling closes the pixels between the discs: the point of the pass
    /// is that a cloud reads as a surface, not as dust.
    #[test]
    fn enhancing_a_cloud_fills_the_gaps_between_its_points() {
        let mesh = sparse_cloud();
        let camera = Camera::default();
        let plain = render(&mesh, &camera, 96, 96, 1, 1.0);
        let enhanced = enhanced(&mesh, &camera, 96);
        let (before, after) = (lit_pixels(&plain), lit_pixels(&enhanced));
        assert!(
            after > before,
            "filling should add pixels: {before} -> {after}"
        );
        // Filling closes gaps; it must not paint the whole frame.
        assert!(after < 96 * 96, "the background must survive: {after}");
    }

    /// Eye-dome lighting only ever darkens, and it does darken something —
    /// otherwise the pass would be a no-op with extra steps.
    #[test]
    fn eye_dome_lighting_darkens_without_brightening() {
        let mesh = sparse_cloud();
        let camera = Camera::default();
        let plain = render_with_scratch(
            &mesh,
            &camera,
            96,
            96,
            1,
            1.0,
            RenderOptions {
                cull_backfaces: false,
                enhance_points: false,
                material_colors: true,
                shadows: false,
                height: HeightField::default(),
            },
            &mut Scratch::default(),
        );
        let enhanced = enhanced(&mesh, &camera, 96);
        let mut darkened = 0;
        for (before, after) in plain
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .zip(enhanced.bgra.as_chunks::<4>().0)
        {
            for channel in 0..3 {
                assert!(
                    after[channel] <= before[channel],
                    "lighting brightened a pixel: {before:?} -> {after:?}"
                );
                if after[channel] < before[channel] {
                    darkened += 1;
                }
            }
        }
        assert!(darkened > 0, "nothing was shaded");
    }

    /// A triangle mesh is left alone: its shading is already a lighting model,
    /// and the pass exists for the scatter a cloud is drawn as.
    #[test]
    fn enhancement_leaves_a_mesh_untouched() {
        let mesh = cube();
        let camera = Camera::default();
        // Both frames share the baseline the helper pins (no shadow map), so
        // the only difference is the enhancement switch.
        let plain = render_with_scratch(
            &mesh,
            &camera,
            64,
            64,
            1,
            1.0,
            RenderOptions {
                cull_backfaces: false,
                enhance_points: false,
                material_colors: true,
                shadows: false,
                height: HeightField::default(),
            },
            &mut Scratch::default(),
        );
        let enhanced = enhanced(&mesh, &camera, 64);
        assert_eq!(plain.bgra, enhanced.bgra);
    }

    /// Culling the back faces of a closed mesh is free: the front faces won
    /// the depth test anyway, so the picture is the one it always was.
    #[test]
    fn culling_the_back_faces_of_a_closed_mesh_changes_nothing() {
        let mesh = closed_cube();
        let camera = Camera::default();
        let options = |cull: bool| RenderOptions {
            cull_backfaces: cull,
            enhance_points: false,
            material_colors: true,
            shadows: false,
            height: HeightField::default(),
        };
        let two_sided = render_with_scratch(
            &mesh,
            &camera,
            96,
            96,
            1,
            1.0,
            options(false),
            &mut Scratch::default(),
        );
        let culled = render_with_scratch(
            &mesh,
            &camera,
            96,
            96,
            1,
            1.0,
            options(true),
            &mut Scratch::default(),
        );
        assert_eq!(two_sided.bgra, culled.bgra);
    }

    /// The other half of that contract: culling an open shell is *not* free.
    /// Seen from behind, its only surface is a back face, and it disappears —
    /// which is why the viewport only asks for culling on a mesh whose winding
    /// says it is closed.
    #[test]
    fn culling_an_open_shell_erases_it_seen_from_behind() {
        // One quad in the XY plane, wound towards +z.
        let quad = load_obj("v -1 -1 0\nv 1 -1 0\nv 1 1 0\nv -1 1 0\nf 1 2 3\nf 1 3 4\n")
            .expect("quad parses");
        assert_eq!(
            quad.winding(),
            crate::media::formats::types::Winding::TwoSided
        );
        // Looking at it from behind: rotate the camera a half turn.
        let camera = Camera {
            yaw: std::f32::consts::PI,
            pitch: 0.0,
            zoom: 1.0,
            pan: [0.0, 0.0],
        };
        let two_sided = render(&quad, &camera, 64, 64, 1, 1.0);
        let culled = render_with_scratch(
            &quad,
            &camera,
            64,
            64,
            1,
            1.0,
            RenderOptions {
                cull_backfaces: true,
                enhance_points: false,
                material_colors: true,
                shadows: false,
                height: HeightField::default(),
            },
            &mut Scratch::default(),
        );
        assert!(lit_pixels(&two_sided) > 100, "the shell is visible");
        assert_eq!(lit_pixels(&culled), 0, "culling left nothing behind it");
    }

    /// The whole point of `Scratch`: a frame of the same size reuses the
    /// buffers instead of allocating 50 MB again, and a bigger frame grows
    /// them rather than corrupting anything.
    #[test]
    fn scratch_buffers_are_reused_between_frames() {
        let mesh = cube();
        let camera = Camera::default();
        let mut scratch = Scratch::default();
        let _ = render_with_scratch(
            &mesh,
            &camera,
            64,
            48,
            1,
            1.0,
            RenderOptions::default(),
            &mut scratch,
        );
        let after_first = scratch.bytes();
        assert!(after_first > 0, "a frame must have used the buffers");

        for _ in 0..4 {
            let frame = render_with_scratch(
                &mesh,
                &camera,
                64,
                48,
                1,
                1.0,
                RenderOptions::default(),
                &mut scratch,
            );
            assert_eq!((frame.width, frame.height), (64, 48));
        }
        assert_eq!(scratch.bytes(), after_first, "the buffers are reused");

        let frame = render_with_scratch(
            &mesh,
            &camera,
            128,
            96,
            1,
            1.0,
            RenderOptions::default(),
            &mut scratch,
        );
        assert_eq!((frame.width, frame.height), (128, 96));
        assert!(scratch.bytes() > after_first, "a bigger frame needs more");
        assert!(!frame.is_empty_of_geometry());
    }

    #[test]
    fn frame_has_the_requested_size_and_is_opaque() {
        let frame = render(&cube(), &Camera::default(), 96, 64, 1, 1.0);
        assert_eq!(frame.width, 96);
        assert_eq!(frame.height, 64);
        assert_eq!(frame.bgra.len(), 96 * 64 * 4);
        assert!(frame.bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn a_cube_covers_part_of_the_frame() {
        let frame = render(&cube(), &Camera::default(), 96, 96, 1, 1.0);
        let lit = lit_pixels(&frame);
        assert!(lit > 200, "expected a visible model, got {lit} pixels");
        assert!(lit < 96 * 96, "the model should not fill the frame");
        assert!(!frame.is_empty_of_geometry());
    }

    #[test]
    fn zooming_in_covers_more_pixels() {
        let mesh = cube();
        let mut camera = Camera::default();
        let wide = lit_pixels(&render(&mesh, &camera, 96, 96, 1, 1.0));
        camera.zoom_by(0.5, MIN_ZOOM, MAX_ZOOM);
        let close = lit_pixels(&render(&mesh, &camera, 96, 96, 1, 1.0));
        assert!(close > wide, "close={close} wide={wide}");
    }

    #[test]
    fn supersampling_keeps_the_output_size() {
        let frame = render(&cube(), &Camera::default(), 64, 48, 2, 1.0);
        assert_eq!((frame.width, frame.height), (64, 48));
        assert_eq!(frame.bgra.len(), 64 * 48 * 4);
    }

    #[test]
    fn the_nearer_triangle_wins_the_depth_test() {
        let backdrop = render(&occluded_quad(false), &Camera::default(), 128, 128, 1, 1.0);
        let occluded = render(&occluded_quad(true), &Camera::default(), 128, 128, 1, 1.0);
        let (near, far) = (
            brightness(occluded.pixel(64, 64)),
            brightness(backdrop.pixel(64, 64)),
        );
        assert!(
            near < far,
            "the small quad in front should hide the bright backdrop: near={near} far={far}"
        );
    }

    #[test]
    fn a_mesh_without_bounds_renders_background_only() {
        let frame = render(&Mesh::default(), &Camera::default(), 64, 64, 1, 1.0);
        assert!(frame.is_empty_of_geometry());
        assert_eq!(lit_pixels(&frame), 0);
    }

    #[test]
    fn pitch_is_clamped_and_yaw_wraps() {
        let mut camera = Camera::default();
        camera.orbit(0.0, 100.0);
        assert_eq!(camera.pitch, MAX_PITCH);
        camera.orbit(0.0, -1000.0);
        assert_eq!(camera.pitch, -MAX_PITCH);

        // Three whole turns plus a quarter: the wrap must leave just the
        // quarter, measured from the default yaw.
        camera.orbit(std::f32::consts::TAU * 3.0 + 0.25, 0.0);
        assert!(camera.yaw > -std::f32::consts::PI && camera.yaw <= std::f32::consts::PI);
        assert!((camera.yaw - 0.87).abs() < 1e-3, "yaw={}", camera.yaw);
    }

    #[test]
    fn yaw_wraps_a_negative_quarter_turn() {
        let mut camera = Camera::default();
        camera.orbit(-std::f32::consts::TAU * 2.0 - 1.0, 0.0);
        assert!((camera.yaw - -0.38).abs() < 1e-3, "yaw={}", camera.yaw);
    }

    #[test]
    fn zoom_and_reset_stay_in_range() {
        let mut camera = Camera::default();
        for _ in 0..50 {
            camera.zoom_by(0.5, MIN_ZOOM, MAX_ZOOM);
        }
        assert_eq!(camera.zoom, MIN_ZOOM);
        for _ in 0..50 {
            camera.zoom_by(2.0, MIN_ZOOM, MAX_ZOOM);
        }
        assert_eq!(camera.zoom, MAX_ZOOM);
        assert!(!camera.is_default());
        camera.reset();
        assert!(camera.is_default());
    }

    #[test]
    fn resolution_is_clamped_to_a_sane_range() {
        assert_eq!(clamp_edge(1), MIN_EDGE);
        assert_eq!(clamp_edge(600), 600);
        assert_eq!(clamp_edge(u32::MAX), MAX_EDGE);
        let tiny = render(&cube(), &Camera::default(), 1, 1, 1, 1.0);
        assert_eq!(tiny.width, MIN_EDGE);
    }

    #[test]
    fn auto_size_shrinks_for_heavy_meshes() {
        assert_eq!(auto_size(1_000, 900), 900);
        assert_eq!(auto_size(300_000, 900), 720);
        assert_eq!(auto_size(2_000_000, 900), 560);
        // A small cap is never exceeded, and the floor never pushes past it.
        assert_eq!(auto_size(2_000_000, 320), 320);
        assert_eq!(auto_size(2_000_000, 200), 200);
    }

    // -- Framing: the contract the GPU backend relies on --------------------

    /// `m * v` for a column-major 4×4, which is how WGSL reads `mat4x4<f32>`.
    fn mat_vec(m: &[[f32; 4]; 4], v: [f32; 4]) -> [f32; 4] {
        let mut out = [0.0f32; 4];
        for (j, column) in m.iter().enumerate() {
            for (i, value) in column.iter().enumerate() {
                out[i] += value * v[j];
            }
        }
        out
    }

    /// Unit-sphere space → file coordinates, the inverse of `to_unit`.
    fn from_unit(framing: &Framing, unit: [f32; 3]) -> [f32; 3] {
        let radius = 1.0 / framing.inv_radius;
        [
            framing.center[0] + unit[0] * radius,
            framing.center[1] + unit[1] * radius,
            framing.center[2] + unit[2] * radius,
        ]
    }

    #[test]
    fn the_gpu_matrix_reproduces_the_cpu_projection() {
        let bounds = cube().bounds;
        let (width, height) = (320.0f32, 240.0f32);
        let mut camera = Camera::default();
        let viewpoints = [
            (0.0f32, 0.0f32, 1.0f32, [0.0f32, 0.0]),
            (0.62, 0.34, 1.0, [0.0, 0.0]),
            (-1.3, -0.8, 0.5, [0.0, 0.0]),
            (2.4, 0.4, 2.0, [0.0, 0.0]),
            (0.0, 1.4, 1.0, [0.0, 0.0]),
            // Panned views too: the matrix carries the pivot, so a pan the CPU
            // path honoured but the matrix did not would put the two pictures
            // side by side.
            (0.62, 0.34, 1.0, [0.35, -0.2]),
            (-2.0, 0.9, 3.5, [-0.6, 0.45]),
        ];
        for (yaw, pitch, zoom, pan) in viewpoints {
            camera.yaw = yaw;
            camera.pitch = pitch;
            camera.zoom = zoom;
            camera.pan = pan;
            let framing = camera.framing(bounds, width / height);
            let matrix = framing.view_projection();
            for p in [
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 1.0],
                [0.5, 0.25, 1.0],
                [1.0, 1.0, 1.0],
            ] {
                let clip = mat_vec(&matrix, [p[0], p[1], p[2], 1.0]);
                assert!(clip[3] > 0.0, "point behind the eye at {yaw}/{pitch}");
                let (screen, inv_z) = framing.to_screen(framing.to_view(p), width, height);

                // The matrix and the CPU path must agree on where a point
                // lands, or the two backends would frame a model differently.
                let expect_x = (clip[0] / clip[3] * 0.5 + 0.5) * width;
                let expect_y = (0.5 - clip[1] / clip[3] * 0.5) * height;
                assert!(
                    (screen[0] - expect_x).abs() < 0.01 && (screen[1] - expect_y).abs() < 0.01,
                    "screen {screen:?} vs matrix ({expect_x}, {expect_y})"
                );
                // `w` is exactly the view-space depth.
                assert!(
                    (clip[3] - 1.0 / inv_z).abs() < 1e-3,
                    "w {} vs depth {}",
                    clip[3],
                    1.0 / inv_z
                );
                assert!((0.0..=1.0).contains(&(clip[2] / clip[3])));
            }
        }
    }

    #[test]
    fn the_depth_range_lands_on_one_and_falls_towards_zero() {
        let framing = Camera::default().framing(cube().bounds, 1.0);
        let matrix = framing.view_projection();
        let (near, far) = framing.depth_range();
        // The reversed projection: `near` maps to exactly one, and depth
        // falls towards zero as the distance grows — halving every time the
        // distance doubles, since the stored value is `near / vz`.
        for (depth, expected) in [(near, 1.0f32), (far, near / far)] {
            let unit = add(framing.eye, scale(framing.forward, depth));
            let p = from_unit(&framing, unit);
            let clip = mat_vec(&matrix, [p[0], p[1], p[2], 1.0]);
            let ndc_z = clip[2] / clip[3];
            assert!(
                (ndc_z - expected).abs() < 1e-3,
                "depth {depth} mapped to {ndc_z}, expected {expected}"
            );
        }
        // And it falls off monotonically: everything nearer the camera
        // stores a larger depth, which is what the GreaterEqual compare
        // relies on.
        let mut previous = 1.1f32;
        for step in [1, 2, 4, 8, 16, 32] {
            let depth = near * step as f32;
            let unit = add(framing.eye, scale(framing.forward, depth));
            let p = from_unit(&framing, unit);
            let clip = mat_vec(&matrix, [p[0], p[1], p[2], 1.0]);
            let ndc_z = clip[2] / clip[3];
            assert!(ndc_z < previous, "depth must fall with distance");
            previous = ndc_z;
        }
    }

    /// The culling frustum must be the exact volume the projection matrix
    /// draws. A frustum built with the transpose the wrong way round, or with
    /// the OpenGL `-1..=1` near/far planes, is still a plausible-looking box;
    /// only comparing it against the matrix catches that.
    #[test]
    fn the_frustum_matches_the_projection_matrix() {
        let bounds = cube().bounds;
        let (width, height) = (640.0f32, 480.0f32);
        let viewpoints = [
            (0.0f32, 0.0f32, 1.0f32, [0.0f32, 0.0]),
            (0.9, 0.5, 1.6, [0.2, -0.1]),
            (-2.2, -0.7, 0.6, [-0.4, 0.3]),
            (3.1, 1.2, 3.0, [0.0, 0.0]),
        ];
        // Points on and around the model, so both outcomes are exercised.
        let points = [
            [0.0f32, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.5, 0.5, 0.5],
            [1.0, 1.0, 1.0],
            [-0.25, 0.5, 1.25],
            [4.0, 0.0, 0.0],
            [0.0, -4.0, 0.0],
            [0.5, 0.5, -5.0],
            [0.5, 0.5, 40.0],
        ];
        for (yaw, pitch, zoom, pan) in viewpoints {
            let camera = Camera {
                yaw,
                pitch,
                zoom,
                pan,
            };
            let framing = camera.framing(bounds, width / height);
            let frustum = framing.frustum();
            let matrix = framing.view_projection();
            for point in points {
                let clip = mat_vec(&matrix, [point[0], point[1], point[2], 1.0]);
                // Skip the boundary, where float error and a `<` vs `<=`
                // decide differently: this test is about which side of the
                // volume a point is on, not about the edge itself.
                if clip[3] < 1e-3 {
                    continue;
                }
                let ndc = [clip[0] / clip[3], clip[1] / clip[3], clip[2] / clip[3]];
                if ndc.iter().any(|v| v.abs() < 1e-3 || (v - 1.0).abs() < 1e-3) {
                    continue;
                }
                let inside = ndc.iter().all(|v| (-1.0..=1.0).contains(v));
                let kept = frustum.intersects_bounds(&Bounds {
                    min: point,
                    max: point,
                });
                assert_eq!(
                    kept, inside,
                    "point {point:?} at yaw {yaw} pitch {pitch}: frustum {kept}, matrix {inside} (ndc {ndc:?})"
                );
            }
        }
    }

    #[test]
    fn framing_keeps_the_model_inside_the_viewport() {
        let bounds = cube().bounds;
        let (width, height) = (400.0f32, 300.0f32);
        for zoom in [1.0f32, 2.0, MIN_ZOOM, MAX_ZOOM] {
            let camera = Camera {
                zoom,
                ..Camera::default()
            };
            let framing = camera.framing(bounds, width / height);
            // Every bounding-box corner, projected.
            let (lo, hi) = (bounds.min, bounds.max);
            let mut screen = Vec::new();
            for x in [lo[0], hi[0]] {
                for y in [lo[1], hi[1]] {
                    for z in [lo[2], hi[2]] {
                        screen.push(
                            framing
                                .to_screen(framing.to_view([x, y, z]), width, height)
                                .0,
                        );
                    }
                }
            }
            let min_x = screen.iter().fold(f32::INFINITY, |a, s| a.min(s[0]));
            let max_x = screen.iter().fold(f32::NEG_INFINITY, |a, s| a.max(s[0]));
            let min_y = screen.iter().fold(f32::INFINITY, |a, s| a.min(s[1]));
            let max_y = screen.iter().fold(f32::NEG_INFINITY, |a, s| a.max(s[1]));
            if zoom >= 1.0 {
                // Framed: the whole model has to fit.
                assert!(
                    min_x >= -0.5 && max_x <= width + 0.5,
                    "x {min_x}..{max_x} zoom {zoom}"
                );
                assert!(
                    min_y >= -0.5 && max_y <= height + 0.5,
                    "y {min_y}..{max_y} zoom {zoom}"
                );
            } else {
                // Zoomed in: it must overflow, or zoom would do nothing.
                assert!(min_x < 0.0 || max_x > width, "zoom {zoom} did not overflow");
            }
        }
    }

    // -- Turning: the sign convention the viewport's drag relies on --------

    /// Where a world point lands, with the camera turned by `(yaw, pitch)`.
    fn turned_screen(point: [f32; 3], yaw: f32, pitch: f32) -> [f32; 2] {
        let bounds = cube().bounds;
        let (width, height) = (400.0f32, 300.0f32);
        let camera = Camera {
            yaw,
            pitch,
            zoom: 1.0,
            pan: [0.0, 0.0],
        };
        let framing = camera.framing(bounds, width / height);
        framing.to_screen(framing.to_view(point), width, height).0
    }

    /// A growing yaw moves the eye towards +x, which swings the model's near
    /// face to the *left*. The viewport drags with a negated yaw so that the
    /// surface follows the pointer — this pins the sign it negates away from.
    #[test]
    fn growing_yaw_swings_the_model_left() {
        // The centre of the cube's +z face, i.e. the surface nearest the eye
        // at the default orientation.
        let near = [0.5, 0.5, 1.0];
        let before = turned_screen(near, 0.0, 0.0);
        let after = turned_screen(near, 0.2, 0.0);
        assert!(
            after[0] < before[0] - 1.0,
            "yaw +0.2 moved the near face right: {before:?} -> {after:?}"
        );
        // And the opposite way for the opposite turn, so the drag is symmetric.
        let back = turned_screen(near, -0.2, 0.0);
        assert!(back[0] > before[0] + 1.0, "{before:?} -> {back:?}");
    }

    /// A growing pitch lifts the eye, which slides the near face *down* the
    /// screen — the direction a downward drag goes, so this axis needs no sign
    /// flip.
    #[test]
    fn growing_pitch_slides_the_model_down() {
        let near = [0.5, 0.5, 1.0];
        let before = turned_screen(near, 0.0, 0.0);
        let after = turned_screen(near, 0.0, 0.2);
        assert!(
            after[1] > before[1] + 1.0,
            "pitch +0.2 moved the near face up: {before:?} -> {after:?}"
        );
    }

    // -- Panning -------------------------------------------------------------

    /// The pan/pixel conversion has to be the one the projection actually
    /// uses, or dragging would move the model a different distance than the
    /// cursor travelled.
    #[test]
    fn pan_per_pixel_matches_the_projection() {
        let bounds = cube().bounds;
        let (width, height) = (400.0f32, 300.0f32);
        let aspect = width / height;
        let camera = Camera::default();
        let world = bounds.center();
        let project = |camera: &Camera| {
            let framing = camera.framing(bounds, aspect);
            framing.to_screen(framing.to_view(world), width, height).0
        };
        let before = project(&camera);

        // Dragging 40 px right means the pivot moves left by those pixels' worth.
        let per_pixel = camera.pan_per_pixel(height);
        let mut panned = camera;
        panned.pan_by([-per_pixel * 40.0, 0.0]);
        let after = project(&panned);
        assert!(
            (after[0] - before[0] - 40.0).abs() < 0.1,
            "expected the model to move 40 px right, moved {}",
            after[0] - before[0]
        );
        assert!((after[1] - before[1]).abs() < 0.1, "y must not move");
    }

    /// Panning up slides the model down the screen — the content follows the
    /// hand, which is why the app adds a downward drag to the pan.
    #[test]
    fn panning_up_moves_the_model_down() {
        let bounds = cube().bounds;
        let (width, height) = (400.0f32, 300.0f32);
        let aspect = width / height;
        let camera = Camera::default();
        let world = bounds.center();
        let project = |camera: &Camera| {
            let framing = camera.framing(bounds, aspect);
            framing.to_screen(framing.to_view(world), width, height).0
        };
        let before = project(&camera);
        let mut panned = camera;
        panned.pan_by([0.0, camera.pan_per_pixel(height) * 25.0]);
        let after = project(&panned);
        assert!(
            (after[1] - before[1] - 25.0).abs() < 0.1,
            "expected 25 px down, moved {}",
            after[1] - before[1]
        );
    }

    /// The point the camera was panned to stays at the centre of the viewport,
    /// which is what makes a later orbit turn around what the user is looking
    /// at instead of around the model's middle.
    #[test]
    fn the_panned_pivot_stays_centred() {
        let bounds = cube().bounds;
        let (width, height) = (400.0f32, 300.0f32);
        let aspect = width / height;
        let mut camera = Camera::default();
        camera.pan_by([0.4, -0.3]);
        let framing = camera.framing(bounds, aspect);
        let (screen, _) = framing.to_screen(framing.to_view(framing.center), width, height);
        assert!((screen[0] - width / 2.0).abs() < 1e-3, "{screen:?}");
        assert!((screen[1] - height / 2.0).abs() < 1e-3, "{screen:?}");

        // ...and it still does after orbiting: the pivot is the orbit centre.
        let mut orbited = camera;
        orbited.orbit(1.7, 0.6);
        let framing = orbited.framing(bounds, aspect);
        let (screen, _) = framing.to_screen(framing.to_view(framing.center), width, height);
        assert!((screen[0] - width / 2.0).abs() < 1e-3, "{screen:?}");
        assert!((screen[1] - height / 2.0).abs() < 1e-3, "{screen:?}");
    }

    /// A pan is a camera move like any other: `reset` undoes it, and it does
    /// not change how far away the eye is.
    #[test]
    fn pan_does_not_change_the_distance_and_reset_undoes_it() {
        let bounds = cube().bounds;
        let camera = Camera::default();
        let mut panned = camera;
        panned.pan_by([0.5, 0.25]);
        assert!(!panned.is_default());
        let before = camera.framing(bounds, 1.0);
        let after = panned.framing(bounds, 1.0);
        assert!((before.distance - after.distance).abs() < 1e-6);
        assert!((before.inv_radius - after.inv_radius).abs() < 1e-6);
        panned.reset();
        assert!(panned.is_default());
    }

    #[test]
    fn the_eye_round_trips_into_model_space() {
        let framing = Camera::default().framing(cube().bounds, 1.0);
        let back = framing.to_unit(framing.eye_in_model_space());
        for (b, e) in back.into_iter().zip(framing.eye) {
            assert!((b - e).abs() < 1e-4);
        }
    }

    // -- GPU vertex layout --------------------------------------------------

    #[test]
    fn vertex_data_keeps_indices_for_smooth_meshes() {
        // A cube with per-vertex normals on every face.
        let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nvn 0 0 1\nf 1//1 2//1 3//1\n";
        let mesh = load_obj(obj).expect("mesh parses");
        let data = vertex_data(&mesh);
        assert_eq!(data.vertex_count, 3);
        assert_eq!(data.vertices.len(), 3 * 6);
        assert_eq!(data.indices.as_deref(), Some([0, 1, 2].as_slice()));
        assert_eq!(data.triangle_count(), 1);
        // Byte views mirror the typed arrays exactly.
        assert_eq!(data.vertex_bytes().len(), 3 * VertexData::STRIDE as usize);
        assert_eq!(data.index_bytes().map(|b| b.len()), Some(12));
    }

    #[test]
    fn vertex_byte_views_round_trip() {
        let values = [1.5f32, -2.25, 0.0];
        let bytes = f32_bytes(&values);
        assert_eq!(bytes.len(), 12);
        for (index, value) in values.iter().enumerate() {
            let at = index * 4;
            assert_eq!(
                f32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap()),
                *value
            );
        }
        let indices = u32_bytes(&[7, 0, 12]);
        assert_eq!(indices.len(), 12);
        assert_eq!(u32::from_ne_bytes(indices[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_ne_bytes(indices[8..12].try_into().unwrap()), 12);
    }

    /// Reversing the winding on the way into the buffer is what lets a closed
    /// mesh be culled whichever way its file happened to spell it.
    #[test]
    fn vertex_data_can_flip_the_winding() {
        let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nvn 0 0 1\nf 1//1 2//1 3//1\n";
        let mesh = load_obj(obj).expect("mesh parses");
        let straight = vertex_data_with(&mesh, false);
        let flipped = vertex_data_with(&mesh, true);
        assert_eq!(straight.indices.as_deref(), Some([0, 1, 2].as_slice()));
        assert_eq!(flipped.indices.as_deref(), Some([0, 2, 1].as_slice()));
        // The normal follows the winding, so the shading keeps up.
        assert_eq!(&straight.vertices[3..6], &[0.0, 0.0, 1.0]);
        assert_eq!(&flipped.vertices[3..6], &[0.0, 0.0, -1.0]);
        assert_eq!(flipped.vertex_count, straight.vertex_count);

        // Flat meshes recompute the face normal from the corners, so the flip
        // has to reach them too.
        let flat = load_obj("v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").expect("mesh parses");
        let straight = vertex_data_with(&flat, false);
        let flipped = vertex_data_with(&flat, true);
        assert_eq!(&straight.vertices[3..6], &[0.0, 0.0, 1.0]);
        assert_eq!(&flipped.vertices[3..6], &[0.0, 0.0, -1.0]);
    }

    #[test]
    fn vertex_data_expands_flat_meshes_with_face_normals() {
        let data = vertex_data(&cube());
        assert!(data.indices.is_none(), "flat meshes expand per face");
        assert_eq!(data.vertex_count, 12 * 3);
        assert_eq!(data.vertices.len(), 12 * 3 * 6);
        assert_eq!(data.triangle_count(), 12);
        // Every corner of a triangle carries the same, unit-length normal,
        // which is perpendicular to the face it belongs to.
        for triangle in data.vertices.as_chunks::<18>().0.iter() {
            let normal = [triangle[3], triangle[4], triangle[5]];
            assert!((dot(normal, normal) - 1.0).abs() < 1e-4);
            for corner in 1..3 {
                let base = corner * 6;
                assert_eq!(
                    [triangle[base + 3], triangle[base + 4], triangle[base + 5]],
                    normal
                );
            }
            let edge_a = sub(
                [triangle[6], triangle[7], triangle[8]],
                [triangle[0], triangle[1], triangle[2]],
            );
            let edge_b = sub(
                [triangle[12], triangle[13], triangle[14]],
                [triangle[0], triangle[1], triangle[2]],
            );
            // Parallel to the corners' own cross product, so it must point
            // along the stored normal.
            let face = normalize(cross(edge_a, edge_b));
            assert!(dot(face, normal).abs() > 0.9999, "face normal mismatch");
        }
    }

    // ---- point clouds ---------------------------------------------------

    /// A cloud built the way a file arrives: a PLY with no `face` element.
    fn cloud(points: &[[f32; 3]]) -> Mesh {
        let mut ply = format!(
            "ply\nformat ascii 1.0\nelement vertex {}\n\
             property float x\nproperty float y\nproperty float z\nend_header\n",
            points.len()
        );
        for p in points {
            ply.push_str(&format!("{} {} {}\n", p[0], p[1], p[2]));
        }
        crate::media::formats::load_ply(ply.as_bytes()).expect("cloud parses")
    }

    /// A cloud whose vertices carry a colour, as a coloured scan arrives.
    fn colored_cloud(points: &[[f32; 3]], colors: &[[u8; 3]]) -> Mesh {
        let mut ply = format!(
            "ply\nformat ascii 1.0\nelement vertex {}\n\
             property float x\nproperty float y\nproperty float z\n\
             property uchar red\nproperty uchar green\nproperty uchar blue\n\
             end_header\n",
            points.len()
        );
        for (point, color) in points.iter().zip(colors) {
            ply.push_str(&format!(
                "{} {} {} {} {} {}\n",
                point[0], point[1], point[2], color[0], color[1], color[2]
            ));
        }
        crate::media::formats::load_ply(ply.as_bytes()).expect("cloud parses")
    }

    /// A scan that carries the two attributes a surveyor's PLY does: return
    /// strength and classification.
    fn scan_cloud(points: &[[f32; 3]], intensities: &[f32], classes: &[u8]) -> Mesh {
        let mut ply = format!(
            "ply\nformat ascii 1.0\nelement vertex {}\n\
             property float x\nproperty float y\nproperty float z\n\
             property float intensity\nproperty uchar classification\n\
             end_header\n",
            points.len()
        );
        for ((point, intensity), class) in points.iter().zip(intensities).zip(classes) {
            ply.push_str(&format!(
                "{} {} {} {} {}\n",
                point[0], point[1], point[2], intensity, class
            ));
        }
        crate::media::formats::load_ply(ply.as_bytes()).expect("scan parses")
    }

    /// Pixels the renderer touched, i.e. everything that left the background.
    fn painted(frame: &Frame) -> usize {
        frame
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[1] < 220)
            .count()
    }

    #[test]
    fn a_point_cloud_renders_without_any_triangle() {
        let mesh = cloud(&[
            [0.0, 0.0, 0.0],
            [0.6, 0.0, 0.0],
            [-0.6, 0.0, 0.0],
            [0.0, 0.6, 0.0],
            [0.0, 0.0, 0.6],
        ]);
        assert!(mesh.is_point_cloud());
        let frame = render(&mesh, &Camera::default(), 64, 64, 1, 1.0);
        assert!(painted(&frame) > 0, "a cloud must paint something");
    }

    /// One point is a small sprite, not a full-screen blob; the count also
    /// proves the disc test is not off by a pixel row.
    #[test]
    fn one_point_paints_a_small_sprite() {
        let mesh = cloud(&[[0.0, 0.0, 0.0]]);
        let frame = render(&mesh, &Camera::default(), 64, 64, 1, 1.0);
        let count = painted(&frame);
        assert!(
            (1..=16).contains(&count),
            "one point covered {count} pixels"
        );
    }

    /// Two points on the same sight line, distinguishable by colour alone —
    /// the studio rig shades both with the same camera-facing normal, so the
    /// centre pixel's colour, not its brightness, says which one won the
    /// depth test.
    #[test]
    fn points_are_depth_tested() {
        let axis_on = Camera {
            yaw: 0.0,
            pitch: 0.0,
            zoom: 1.0,
            pan: [0.0, 0.0],
        };
        let mesh = colored_cloud(
            &[[0.0, 0.0, 1.0], [0.0, 0.0, -1.0]],
            &[[255, 0, 0], [0, 0, 255]],
        );
        let frame = render(&mesh, &axis_on, 64, 64, 1, 1.0);
        let centre = frame.pixel(32, 32);
        // BGRA: the nearer point is the red one.
        assert!(
            centre[2] > centre[0] + centre[1],
            "the nearer point must win the depth test: pixel {centre:?}"
        );
    }

    #[test]
    fn a_cloud_without_normals_is_lit_from_its_own_centre() {
        let mesh = cloud(&[[2.0, 0.0, 0.0], [0.0, 0.0, 0.0], [-2.0, 0.0, 0.0]]);
        assert!(!mesh.has_vertex_normals());
        assert_eq!(point_normal(&mesh, 0), [1.0, 0.0, 0.0]);
        assert_eq!(point_normal(&mesh, 2), [-1.0, 0.0, 0.0]);
        // The centre point has no direction of its own: any unit direction
        // would do, and it falls to the fixed one.
        assert_eq!(point_normal(&mesh, 1), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn the_instance_data_carries_a_normal_and_a_colour_per_point() {
        let mesh = cloud(&[[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0]]);
        let data = point_data(&mesh);
        assert_eq!(data.count, 2);
        assert_eq!(data.points.len(), 22);
        assert_eq!(PointData::STRIDE, 3 * 3 * 4 + 2 * 4);
        // The file carries no colour, so both points fall back to the material.
        // It carries no channels either, and the last two floats of a point are
        // then the inert zero rather than a hole in the buffer.
        assert_eq!(
            &data.points[0..11],
            &[
                1.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                MATERIAL[0],
                MATERIAL[1],
                MATERIAL[2],
                0.0,
                0.0
            ]
        );
        assert_eq!(
            &data.points[11..22],
            &[
                -1.0,
                0.0,
                0.0,
                -1.0,
                0.0,
                0.0,
                MATERIAL[0],
                MATERIAL[1],
                MATERIAL[2],
                0.0,
                0.0
            ]
        );
    }

    /// The two scanner attributes reach the buffer the shader reads, at the slots
    /// the WGSL's vertex inputs name, and reach the CPU colouring the same way.
    #[test]
    fn a_clouds_channels_reach_both_renderers() {
        let mesh = scan_cloud(
            &[[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0]],
            &[234.0, 12.0],
            &[6, 2],
        );
        assert_eq!(mesh.intensity_range(), Some((12.0, 234.0)));
        assert_eq!(mesh.class_count(), Some(7));

        let data = point_data(&mesh);
        assert_eq!(&data.points[9..11], &[234.0, 6.0]);
        assert_eq!(&data.points[20..22], &[12.0, 2.0]);

        // The CPU path reads the same arrays through the same lookup, so an
        // intensity of 234 is the top of the scale for this cloud.
        let look = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Intensity,
            scale: Scale::Preset(scale_by_id("grey")),
            ..Default::default()
        };
        let tinted = look.resolve(&FieldData {
            bounds: &mesh.bounds,
            intensities: mesh.intensity_range(),
            classes: None,
        });
        assert_eq!(
            tinted.tint_at(&Sample::of(&mesh, 0, [1.0, 0.0, 0.0])),
            Some([1.0; 3])
        );
        assert_eq!(
            tinted.tint_at(&Sample::of(&mesh, 1, [-1.0, 0.0, 0.0])),
            Some([0.0; 3])
        );
    }

    /// A scan that carries its own colours is painted with them, not with the
    /// material: the point renders in the file's colour.
    #[test]
    fn a_coloured_cloud_paints_with_its_own_colours() {
        let mesh = colored_cloud(&[[0.0, 0.0, 0.0]], &[[255, 0, 0]]);
        assert!(mesh.has_vertex_colors());
        assert_eq!(base_color(&mesh, 0), [1.0, 0.0, 0.0]);

        let frame = render(&mesh, &Camera::default(), 64, 64, 1, 1.0);
        // BGRA: a red point leaves the green and blue channels dark, where a
        // material-shaded one would be a grey-blue. The exact red depends on
        // the lighting; dominance is the part that must not drift.
        let centre = frame.pixel(32, 32);
        assert!(
            centre[2] > 80 && centre[2] > centre[0] && centre[2] > centre[1],
            "expected a red point, got {centre:?}"
        );

        // The instance data carries the same colour the CPU splat used.
        let data = point_data(&mesh);
        assert_eq!(&data.points[6..9], &[1.0, 0.0, 0.0]);
    }

    /// A cloud of one repeated point has no extent at all; the framing has to
    /// survive that and still draw the sprite.
    #[test]
    fn a_degenerate_cloud_still_paints_its_sprite() {
        let mesh = cloud(&[[1.0, 1.0, 1.0], [1.0, 1.0, 1.0]]);
        assert_eq!(mesh.bounds.longest_edge(), 0.0);
        assert!(painted(&render(&mesh, &Camera::default(), 64, 64, 1, 1.0)) > 0);
    }
}
