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
//! 4. shading is two-sided Lambert plus a Blinn-Phong highlight — flat when the
//!    file carried no normals, Gouraud when it did.
//!
//! The key light is fixed in model space, so orbiting the camera changes the
//! shading across the surface: the model reads as a solid object turning,
//! rather than a silhouette under a headlight.

use super::formats::point_cloud::Frustum;
use super::formats::types::{Bounds, Mesh};

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

/// Key light direction in model space (normalised on use): up and to the left,
/// so the default camera — up and to the right — sees a lit face.
pub const KEY_LIGHT: [f32; 3] = [-0.45, 0.78, 0.52];
/// Background gradient: brighter at the top, with a soft corner vignette.
pub const BG_TOP: [f32; 3] = [0.965, 0.969, 0.976];
pub const BG_BOTTOM: [f32; 3] = [0.869, 0.882, 0.898];
/// Neutral cool-grey surface.
pub const MATERIAL: [f32; 3] = [0.600, 0.645, 0.710];
pub const AMBIENT: f32 = 0.30;
pub const DIFFUSE: f32 = 0.70;
pub const SPECULAR: f32 = 0.20;
pub const SHININESS: f32 = 40.0;
/// Fraction of the image the corner vignette darkens by.
pub const VIGNETTE: f32 = 0.35;
/// Radius of one point sprite, in pixels of the final image. Both renderers
/// draw a cloud as discs of this size; it is a uniform because the GPU shader
/// bakes in no constants of its own.
pub const POINT_RADIUS: f32 = 1.15;

/// Height band size for the elevation colouring, in model units.
///
/// Every this many units of model-space `y` gets its own hue, so the count of
/// bands follows the file's real scale rather than its pixel size.
pub const HEIGHT_BAND: f32 = 10.0;

/// Axis gizmo colours: X, Y, Z, the usual red/green/blue.
pub const AXIS_X: [f32; 3] = [0.87, 0.28, 0.28];
pub const AXIS_Y: [f32; 3] = [0.30, 0.74, 0.34];
pub const AXIS_Z: [f32; 3] = [0.30, 0.47, 0.90];

/// Hue step between neighbouring height bands, in turns.
///
/// The golden angle, so neighbouring bands are as far apart as possible and
/// the sequence keeps finding new hues instead of cycling after a handful.
const BAND_HUE_STEP: f32 = 0.618_034;

/// `HSV(hue, 1, 1)` as RGB, with the hue `t` in turns.
fn hue_rgb(t: f32) -> [f32; 3] {
    let h = t.rem_euclid(1.0) * 6.0;
    let x = 1.0 - (h.rem_euclid(2.0) - 1.0).abs();
    match h as u32 {
        0 => [1.0, x, 0.0],
        1 => [x, 1.0, 0.0],
        2 => [0.0, 1.0, x],
        3 => [0.0, x, 1.0],
        4 => [x, 0.0, 1.0],
        _ => [1.0, 0.0, x],
    }
}

/// The colour one height band is painted with.
///
/// Each [`HEIGHT_BAND`] units of model-space `y` gets its own hue, counted
/// from the model's own floor (`base`) so the first band starts at the model's
/// feet rather than at world zero. `gpu3d.wgsl` mirrors this function, which
/// is what keeps a thumbnail and its viewport frame the same picture.
pub fn height_tint(y: f32, base: f32) -> [f32; 3] {
    hue_rgb(((y - base) / HEIGHT_BAND).floor() * BAND_HUE_STEP)
}

/// Pack the height-colouring / axis uniform into its four floats.
///
/// `x` = colouring on (1) or off (0), `y` = the band size, `z` = the model's
/// floor (where the bands count from), `w` = draw the axis gizmo (1) or not.
/// The GPU reads this as the `bands` vector, so the packing lives here rather
/// than at the call site.
pub fn bands_uniform(height_color: bool, show_axes: bool, base_y: f32) -> [f32; 4] {
    [
        if height_color { 1.0 } else { 0.0 },
        HEIGHT_BAND,
        base_y,
        if show_axes { 1.0 } else { 0.0 },
    ]
}

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
}

impl Scratch {
    /// Bytes currently held, for the viewport's own accounting and tests.
    pub fn bytes(&self) -> usize {
        self.colors.len() * std::mem::size_of::<[f32; 3]>()
            + self.depth.len() * std::mem::size_of::<f32>()
            + (self.model.len() + self.view.len()) * std::mem::size_of::<[f32; 3]>()
            + self.log_depth.len() * std::mem::size_of::<f32>()
    }
}

/// Optional work the rasteriser can do on top of a raw frame.
///
/// Both default to off, so [`render`] — and every caller that did not ask —
/// keeps producing exactly the frame it always did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    /// Paint the surface by height instead of the flat material colour.
    ///
    /// Every [`HEIGHT_BAND`] units of model-space `y` gets its own hue, which
    /// turns a mesh into a readable elevation map. Off by default, so nothing
    /// that did not ask for it keeps the picture it always had.
    pub height_color: bool,
    /// Draw the X/Y/Z axis gizmo on the bounding box's floor corner.
    pub show_axes: bool,
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

    /// Near and far planes in unit-sphere space.
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
    /// `mat4x4<f32>`. Depth maps to `0..1` (wgpu's convention).
    pub fn view_projection(&self) -> [[f32; 4]; 4] {
        let (near, far) = self.depth_range();
        // x_clip = a * vx, y_clip = b * vy, w_clip = vz, and z_clip is affine
        // in vz so that `near..far` lands on `0..1`.
        let a = 1.0 / (self.tan_half * self.aspect);
        let b = 1.0 / self.tan_half;
        let z_scale = far / (far - near);
        let z_bias = -far * near / (far - near);

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
        let zk = row(self.forward, z_scale);
        // z is the same linear form as w, rescaled, plus the constant bias.
        let z = [zk[0], zk[1], zk[2], zk[3] + z_bias];

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
    /// Triangle indices, or `None` when the mesh had to be expanded per face.
    pub indices: Option<Vec<u32>>,
    /// Vertices in the buffer, i.e. `vertices.len() / 6`.
    pub vertex_count: u32,
}

impl VertexData {
    /// Bytes of one interleaved vertex: two `vec3<f32>`.
    pub const STRIDE: u64 = 24;

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
    if mesh.has_vertex_normals() {
        vertices.reserve(mesh.positions.len() * 6);
        for (p, n) in mesh.positions.iter().zip(mesh.normals.iter()) {
            // A flipped winding means the file's normals point the other way
            // too, or the shading would disagree with the culling.
            let n = if flip_winding { neg(*n) } else { *n };
            let n = normalize(n);
            vertices.extend_from_slice(&[p[0], p[1], p[2], n[0], n[1], n[2]]);
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
            indices: Some(indices),
            vertex_count: mesh.positions.len() as u32,
        }
    } else {
        vertices.reserve(mesh.triangles.len() * 18);
        for triangle in &mesh.triangles {
            let corners = if flip_winding {
                [
                    mesh.positions[triangle[0] as usize],
                    mesh.positions[triangle[2] as usize],
                    mesh.positions[triangle[1] as usize],
                ]
            } else {
                [
                    mesh.positions[triangle[0] as usize],
                    mesh.positions[triangle[1] as usize],
                    mesh.positions[triangle[2] as usize],
                ]
            };
            let n = normalize(cross(
                sub(corners[1], corners[0]),
                sub(corners[2], corners[0]),
            ));
            for p in corners {
                vertices.extend_from_slice(&[p[0], p[1], p[2], n[0], n[1], n[2]]);
            }
        }
        VertexData {
            vertex_count: (mesh.triangles.len() * 3) as u32,
            vertices,
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
    /// Interleaved `[x, y, z, nx, ny, nz, r, g, b]` per point.
    pub points: Vec<f32>,
    /// Points in the buffer, i.e. `points.len() / 9`.
    pub count: u32,
}

impl PointData {
    /// Bytes of one interleaved point: position, normal and base colour.
    pub const STRIDE: u64 = 3 * 3 * 4;

    /// The point array as bytes, ready for `Queue::write_buffer`.
    pub fn bytes(&self) -> Vec<u8> {
        f32_bytes(&self.points)
    }
}

/// Flatten a point cloud into the instance data its pipeline reads.
pub fn point_data(mesh: &Mesh) -> PointData {
    let mut points = Vec::with_capacity(mesh.positions.len() * 9);
    for (index, position) in mesh.positions.iter().enumerate() {
        let normal = point_normal(mesh, index);
        let color = base_color(mesh, index);
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
        };
        paint_points(
            &mut target,
            mesh,
            point_radius,
            quality,
            options.height_color,
        );
        if options.show_axes {
            target.paint_axes(mesh);
        }
        return;
    }
    if mesh.triangles.is_empty() {
        return;
    }

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

    let light = normalize(KEY_LIGHT);
    let eye = framing.eye;
    let (near, _) = framing.depth_range();
    let vertex_count = mesh.positions.len();
    let gouraud = mesh.has_vertex_normals();
    // Height bands count from the model's own floor, not from world zero.
    let base_y = mesh.bounds.min[1];

    {
        let mut target = Target {
            colors,
            depth,
            width: w,
            height: h,
            framing,
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
            if options.cull_backfaces && facing_away {
                continue;
            }
            let shaded_face = if facing_away { neg(face) } else { face };
            let half = normalize(add(light, to_camera));
            let spec = SPECULAR * dot(shaded_face, half).max(0.0).powf(SHININESS);

            let mut corners = [Vertex::default(); 3];
            for (slot, index) in [i0, i1, i2].into_iter().enumerate() {
                let normal = if gouraud {
                    let n = normalize(mesh.normals[index]);
                    if n == [0.0; 3] {
                        shaded_face
                    } else if dot(n, to_camera) < 0.0 {
                        neg(n)
                    } else {
                        n
                    }
                } else {
                    shaded_face
                };
                // Height colouring is decided per vertex, so a band boundary
                // lands on the geometry rather than on a pixel; the shader
                // does the same from the same model-space `y`.
                let c = if options.height_color {
                    height_tint(mesh.positions[index][1], base_y)
                } else {
                    MATERIAL
                };
                corners[slot] = Vertex {
                    p: view[index],
                    i: AMBIENT + DIFFUSE * dot(normal, light).max(0.0),
                    c,
                };
            }

            let mut polygon = [Vertex::default(); 4];
            let count = clip_near(corners, near, &mut polygon);
            for k in 1..count.saturating_sub(1) {
                target.triangle(polygon[0], polygon[k], polygon[k + 1], spec);
            }
        }

        // After the model, so the gizmo depth-tests against it and shows
        // wherever the model is not in the way.
        if options.show_axes {
            target.paint_axes(mesh);
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
    height_color: bool,
) {
    let (near, _) = target.framing.depth_range();
    let eye = target.framing.eye_in_model_space();
    let light = normalize(KEY_LIGHT);
    let step = (1.0f32 / quality).round().max(1.0) as usize;

    for (index, position) in mesh.positions.iter().enumerate() {
        if index % step != 0 {
            continue;
        }
        let view = target.framing.to_view(*position);
        if view[2] <= near {
            continue; // Behind the eye, or inside the near plane.
        }
        let normal = point_normal(mesh, index);
        let to_eye = normalize(sub(eye, *position));
        // Two-sided, exactly as the triangle path is.
        let normal = if dot(normal, to_eye) < 0.0 {
            neg(normal)
        } else {
            normal
        };
        let intensity = AMBIENT + DIFFUSE * dot(normal, light).max(0.0);
        // Height colouring wins over the file's own colours; that is the
        // whole point of switching it on.
        let base = if height_color {
            height_tint(position[1], mesh.bounds.min[1])
        } else {
            base_color(mesh, index)
        };
        let rgb = [
            base[0] * intensity,
            base[1] * intensity,
            base[2] * intensity,
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
    if mesh.has_vertex_normals() {
        let n = normalize(mesh.normals[index]);
        return if n == [0.0; 3] {
            normalize(KEY_LIGHT)
        } else {
            n
        };
    }
    let Some(position) = mesh.positions.get(index) else {
        return normalize(KEY_LIGHT);
    };
    let direction = normalize(sub(*position, mesh.bounds.center()));
    if direction == [0.0; 3] {
        normalize(KEY_LIGHT)
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
}

impl Target<'_> {
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
            c: v.c,
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
                self.colors[index] = rgb;
            }
        }
    }

    /// Rasterise one projected triangle with an edge-function test.
    fn triangle(&mut self, a: Vertex, b: Vertex, c: Vertex, spec: f32) {
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
                let intensity = w0 * a.i + w1 * b.i + w2 * c.i;
                self.colors[index] = [
                    (w0 * a.c[0] + w1 * b.c[0] + w2 * c.c[0]) * intensity + spec,
                    (w0 * a.c[1] + w1 * b.c[1] + w2 * c.c[1]) * intensity + spec,
                    (w0 * a.c[2] + w1 * b.c[2] + w2 * c.c[2]) * intensity + spec,
                ];
            }
        }
    }

    /// The X/Y/Z gizmo, as unlit triangles through the same rasteriser and
    /// depth test as the model — so it belongs to the scene rather than
    /// floating over it.
    fn paint_axes(&mut self, mesh: &Mesh) {
        let triangles = axis_triangles(mesh);
        for tri in triangles.as_chunks::<3>().0 {
            // Intensity 1 and no specular is what makes it unlit: a
            // measurement aid reads better flat, and it keeps the GPU's axis
            // pass, which has no lighting at all, in agreement.
            let view: Vec<Vertex> = tri
                .iter()
                .map(|(p, c)| Vertex {
                    p: self.framing.to_view(*p),
                    i: 1.0,
                    c: *c,
                })
                .collect();
            self.triangle(view[0], view[1], view[2], 0.0);
        }
    }
}

/// The X/Y/Z gizmo as flat triangles, `(position, colour)` per vertex with
/// three vertices per triangle.
///
/// Shared by both renderers — the CPU rasterises this list, the GPU uploads
/// it — so the gizmo cannot drift between the two pictures. The rods rise from
/// the bounding box's floor corner: X and Z reach equally far so the floor
/// reads square, and Y spans the model's full height so it doubles as the
/// height-band legend.
pub fn axis_triangles(mesh: &Mesh) -> Vec<([f32; 3], [f32; 3])> {
    let bounds = mesh.bounds;
    if bounds.is_empty() {
        return Vec::new();
    }
    let origin = bounds.min;
    let span = sub(bounds.max, bounds.min);
    let floor = span[0].max(span[2]).max(1e-6);
    let height = span[1].max(1e-6);
    let half = floor.max(height) * 0.006;
    let mut out = Vec::with_capacity(3 * 8 * 3);
    for (tip, color) in [
        (add(origin, [floor, 0.0, 0.0]), AXIS_X),
        (add(origin, [0.0, height, 0.0]), AXIS_Y),
        (add(origin, [0.0, 0.0, floor]), AXIS_Z),
    ] {
        push_rod(&mut out, origin, tip, half, color);
    }
    out
}

/// Append one square rod from `a` to `b` as eight triangles.
fn push_rod(
    out: &mut Vec<([f32; 3], [f32; 3])>,
    a: [f32; 3],
    b: [f32; 3],
    half: f32,
    color: [f32; 3],
) {
    let dir = normalize(sub(b, a));
    // Any axis not parallel to the rod; a vertical rod is the one case where
    // the Y slot degenerates, hence the flip.
    let seed = if dir[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let u = normalize(cross(dir, seed));
    let v = cross(dir, u);
    let ring = |p: [f32; 3]| {
        let corner = |du: f32, dv: f32| add(p, add(scale(u, du * half), scale(v, dv * half)));
        [
            corner(-1.0, -1.0),
            corner(1.0, -1.0),
            corner(1.0, 1.0),
            corner(-1.0, 1.0),
        ]
    };
    let (ra, rb) = (ring(a), ring(b));
    for i in 0..4 {
        let j = (i + 1) % 4;
        for corner in [ra[i], ra[j], rb[j]] {
            out.push((corner, color));
        }
        for corner in [ra[i], rb[j], rb[i]] {
            out.push((corner, color));
        }
    }
}

/// A view-space vertex; `i` is the shading intensity and `c` the base colour,
/// both carried through clipping and interpolated per pixel.
#[derive(Clone, Copy, Default)]
struct Vertex {
    /// x right, y up, z forward.
    p: [f32; 3],
    i: f32,
    /// Base surface colour: the material, a height band, or an axis colour.
    c: [f32; 3],
}

/// A projected vertex.
#[derive(Clone, Copy)]
struct ScreenVertex {
    x: f32,
    y: f32,
    /// `1/z`, linear in screen space and larger when nearer.
    inv_z: f32,
    i: f32,
    c: [f32; 3],
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
            out[count] = Vertex {
                p: lerp3(current.p, next.p, t),
                i: current.i + (next.i - current.i) * t,
                c: [
                    current.c[0] + (next.c[0] - current.c[0]) * t,
                    current.c[1] + (next.c[1] - current.c[1]) * t,
                    current.c[2] + (next.c[2] - current.c[2]) * t,
                ],
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

/// Quantise the linear buffer into BGRA bytes, fully opaque.
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

/// Clamp and scale one linear channel to a byte.
fn to_byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
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

    fn brightness(p: [u8; 4]) -> u32 {
        p[0] as u32 + p[1] as u32 + p[2] as u32
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
                height_color: false,
                show_axes: false,
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
        let plain = render(&mesh, &camera, 96, 96, 1, 1.0);
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
        let plain = render(&mesh, &camera, 64, 64, 1, 1.0);
        let enhanced = enhanced(&mesh, &camera, 64);
        assert_eq!(plain.bgra, enhanced.bgra);
    }

    /// Culling the back faces of a closed mesh is free: the front faces won
    /// the depth test anyway, so the picture is the one it always was.
    #[test]
    fn culling_the_back_faces_of_a_closed_mesh_changes_nothing() {
        let mesh = closed_cube();
        let camera = Camera::default();
        let two_sided = render(&mesh, &camera, 96, 96, 1, 1.0);
        let culled = render_with_scratch(
            &mesh,
            &camera,
            96,
            96,
            1,
            1.0,
            RenderOptions {
                cull_backfaces: true,
                enhance_points: false,
                height_color: false,
                show_axes: false,
            },
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
                height_color: false,
                show_axes: false,
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
    fn a_height_band_keeps_one_colour_and_its_neighbour_differs() {
        let base = 100.0;
        // Constant inside one band...
        assert_eq!(height_tint(base, base), height_tint(base + 9.9, base));
        // ...and a different colour in the next.
        assert_ne!(height_tint(base, base), height_tint(base + 10.0, base));
        // Bands count from the model's own floor, not from world zero.
        assert_eq!(height_tint(0.0, 0.0), height_tint(base, base));
    }

    #[test]
    fn the_axis_gizmo_spans_the_bounds_in_three_colours() {
        let mesh = Mesh {
            positions: vec![[0.0, 0.0, 0.0], [3.0, 4.0, 5.0]],
            triangles: Vec::new(),
            bounds: Bounds {
                min: [0.0; 3],
                max: [3.0, 4.0, 5.0],
            },
            ..Mesh::default()
        };
        let triangles = axis_triangles(&mesh);
        assert!(!triangles.is_empty());
        assert_eq!(triangles.len() % 3, 0, "three vertices per triangle");
        // Every vertex carries one of the three axis colours.
        for (_, color) in &triangles {
            assert!(
                [AXIS_X, AXIS_Y, AXIS_Z].contains(color),
                "unexpected colour {color:?}"
            );
        }
        // The rods stay inside the bounding box, and the Y rod reaches the
        // model's top — which is what makes it a height legend.
        let top = triangles.iter().map(|(p, _)| p[1]).fold(f32::MIN, f32::max);
        assert!(top >= 3.9, "the Y rod reaches the top, got {top}");
        for (p, _) in &triangles {
            assert!(
                p.iter().all(|c| *c >= -0.1 && *c <= 5.1),
                "inside bounds: {p:?}"
            );
        }
    }

    #[test]
    fn a_mesh_with_no_bounds_has_no_axis() {
        let mesh = Mesh {
            bounds: Bounds::empty(),
            ..Mesh::default()
        };
        assert!(axis_triangles(&mesh).is_empty());
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
    fn the_depth_range_lands_on_zero_and_one() {
        let framing = Camera::default().framing(cube().bounds, 1.0);
        let matrix = framing.view_projection();
        let (near, far) = framing.depth_range();
        for (depth, expected) in [(near, 0.0f32), (far, 1.0f32)] {
            let unit = add(framing.eye, scale(framing.forward, depth));
            let p = from_unit(&framing, unit);
            let clip = mat_vec(&matrix, [p[0], p[1], p[2], 1.0]);
            let ndc_z = clip[2] / clip[3];
            assert!(
                (ndc_z - expected).abs() < 1e-3,
                "depth {depth} mapped to {ndc_z}, expected {expected}"
            );
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

    /// Two points on the same sight line, seen head-on. The nearer one is lit
    /// by its outward normal, the far one faces away from the key light and
    /// shades at ambient only, so the pixel between them tells which one won
    /// the depth test.
    #[test]
    fn points_are_depth_tested() {
        let axis_on = Camera {
            yaw: 0.0,
            pitch: 0.0,
            zoom: 1.0,
            pan: [0.0, 0.0],
        };
        let mesh = cloud(&[[0.0, 0.0, 1.0], [0.0, 0.0, -1.0]]);
        let frame = render(&mesh, &axis_on, 64, 64, 1, 1.0);
        let centre = frame.pixel(32, 32);

        let near = MATERIAL[1] * (AMBIENT + DIFFUSE * dot(normalize(KEY_LIGHT), [0.0, 0.0, 1.0]));
        let far = MATERIAL[1] * AMBIENT;
        let blue = centre[0] as f32 / 255.0;
        assert!(
            (blue - near).abs() < (blue - far).abs(),
            "the nearer point must win: pixel {blue} vs near {near} / far {far}"
        );
    }

    #[test]
    fn a_cloud_without_normals_is_lit_from_its_own_centre() {
        let mesh = cloud(&[[2.0, 0.0, 0.0], [0.0, 0.0, 0.0], [-2.0, 0.0, 0.0]]);
        assert!(!mesh.has_vertex_normals());
        assert_eq!(point_normal(&mesh, 0), [1.0, 0.0, 0.0]);
        assert_eq!(point_normal(&mesh, 2), [-1.0, 0.0, 0.0]);
        // The centre point has no direction of its own: lit head-on.
        assert_eq!(point_normal(&mesh, 1), normalize(KEY_LIGHT));
    }

    #[test]
    fn the_instance_data_carries_a_normal_and_a_colour_per_point() {
        let mesh = cloud(&[[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0]]);
        let data = point_data(&mesh);
        assert_eq!(data.count, 2);
        assert_eq!(data.points.len(), 18);
        assert_eq!(PointData::STRIDE, 3 * 3 * 4);
        // The file carries no colour, so both points fall back to the material.
        assert_eq!(
            &data.points[0..9],
            &[
                1.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                MATERIAL[0],
                MATERIAL[1],
                MATERIAL[2]
            ]
        );
        assert_eq!(
            &data.points[9..18],
            &[
                -1.0,
                0.0,
                0.0,
                -1.0,
                0.0,
                0.0,
                MATERIAL[0],
                MATERIAL[1],
                MATERIAL[2]
            ]
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
        // material-shaded one would be a grey-blue.
        let centre = frame.pixel(32, 32);
        assert!(
            centre[2] > 200 && centre[0] < 40 && centre[1] < 40,
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
