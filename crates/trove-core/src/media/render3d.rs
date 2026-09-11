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

use super::mesh::{Bounds, Mesh};

/// Vertical field of view used to frame a model, in degrees.
const FOV_DEG: f32 = 35.0;
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

/// An orbiting camera aimed at the centre of the model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// Rotation around the model's up axis, in radians.
    pub yaw: f32,
    /// Elevation in radians, clamped to ±[`MAX_PITCH`].
    pub pitch: f32,
    /// Distance multiplier; 1.0 frames the whole model.
    pub zoom: f32,
}

impl Default for Camera {
    /// A three-quarter view, which shows both the top and one side.
    fn default() -> Self {
        Self {
            yaw: 0.62,
            pitch: 0.34,
            zoom: 1.0,
        }
    }
}

impl Camera {
    /// Turn the camera by a delta in radians, wrapping yaw and clamping pitch.
    pub fn orbit(&mut self, delta_yaw: f32, delta_pitch: f32) {
        self.yaw = wrap_angle(self.yaw + delta_yaw);
        self.pitch = (self.pitch + delta_pitch).clamp(-MAX_PITCH, MAX_PITCH);
    }

    /// Scale the distance; a factor below 1 moves closer.
    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
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
pub fn render(mesh: &Mesh, camera: &Camera, width: u32, height: u32, supersample: u32) -> Frame {
    let width = clamp_edge(width);
    let height = clamp_edge(height);
    let ss = supersample.clamp(1, MAX_SUPERSAMPLE);
    // Sprites are sized in final-image pixels, so the supersampled buffer
    // needs them scaled up or a cloud would come out thinner after the filter.
    let colors = paint(
        mesh,
        camera,
        width * ss,
        height * ss,
        POINT_RADIUS * ss as f32,
    );
    let colors = if ss > 1 {
        downsample(
            &colors,
            (width * ss) as usize,
            (height * ss) as usize,
            width as usize,
            height as usize,
        )
    } else {
        colors
    };
    Frame {
        width,
        height,
        bgra: to_bgra(&colors),
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
    /// Centre of the model's bounding sphere, in file coordinates.
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
        Framing {
            center,
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
    let mut vertices = Vec::new();
    if mesh.has_vertex_normals() {
        vertices.reserve(mesh.positions.len() * 6);
        for (p, n) in mesh.positions.iter().zip(mesh.normals.iter()) {
            let n = normalize(*n);
            vertices.extend_from_slice(&[p[0], p[1], p[2], n[0], n[1], n[2]]);
        }
        let mut indices = Vec::with_capacity(mesh.triangles.len() * 3);
        for triangle in &mesh.triangles {
            indices.extend_from_slice(triangle);
        }
        VertexData {
            vertices,
            indices: Some(indices),
            vertex_count: mesh.positions.len() as u32,
        }
    } else {
        vertices.reserve(mesh.triangles.len() * 18);
        for triangle in &mesh.triangles {
            let corners = [
                mesh.positions[triangle[0] as usize],
                mesh.positions[triangle[1] as usize],
                mesh.positions[triangle[2] as usize],
            ];
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

/// Rasterise into a linear colour buffer, background included.
fn paint(
    mesh: &Mesh,
    camera: &Camera,
    width: u32,
    height: u32,
    point_radius: f32,
) -> Vec<[f32; 3]> {
    let w = width as usize;
    let h = height as usize;
    let mut colors = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            colors.push(background(x, y, w, h));
        }
    }
    let longest = mesh.bounds.longest_edge();
    if !longest.is_finite() {
        return colors;
    }
    if longest <= 0.0 && !mesh.is_point_cloud() {
        return colors;
    }

    // Normalise into a unit bounding sphere so the camera maths never depends
    // on the file's real units.
    let framing = camera.framing(mesh.bounds, w as f32 / h as f32);
    let mut depth = vec![0f32; w * h];

    if mesh.is_point_cloud() {
        paint_points(&mut colors, &mut depth, mesh, framing, w, h, point_radius);
        return colors;
    }
    if mesh.triangles.is_empty() {
        return colors;
    }

    // Model-space positions drive the shading, view-space positions the
    // rasterizer; both come out of a single pass over the vertices.
    let mut model = Vec::with_capacity(mesh.positions.len());
    let mut view = Vec::with_capacity(mesh.positions.len());
    for p in &mesh.positions {
        model.push(framing.to_unit(*p));
        view.push(framing.to_view(*p));
    }

    let light = normalize(KEY_LIGHT);
    let eye = framing.eye;
    let (near, _) = framing.depth_range();
    let vertex_count = mesh.positions.len();
    let gouraud = mesh.has_vertex_normals();

    {
        let mut target = Target {
            colors: &mut colors,
            depth: &mut depth,
            width: w,
            height: h,
            framing,
        };

        for triangle in &mesh.triangles {
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
            let shaded_face = if dot(face, to_camera) < 0.0 {
                neg(face)
            } else {
                face
            };
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
                corners[slot] = Vertex {
                    p: view[index],
                    i: AMBIENT + DIFFUSE * dot(normal, light).max(0.0),
                };
            }

            let mut polygon = [Vertex::default(); 4];
            let count = clip_near(corners, near, &mut polygon);
            for k in 1..count.saturating_sub(1) {
                target.triangle(polygon[0], polygon[k], polygon[k + 1], spec);
            }
        }
    }

    colors
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
    colors: &mut [[f32; 3]],
    depth: &mut [f32],
    mesh: &Mesh,
    framing: Framing,
    width: usize,
    height: usize,
    radius: f32,
) {
    let (near, _) = framing.depth_range();
    let eye = framing.eye_in_model_space();
    let light = normalize(KEY_LIGHT);

    let mut target = Target {
        colors,
        depth,
        width,
        height,
        framing,
    };

    for (index, position) in mesh.positions.iter().enumerate() {
        let view = framing.to_view(*position);
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
        let base = base_color(mesh, index);
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
                    MATERIAL[0] * intensity + spec,
                    MATERIAL[1] * intensity + spec,
                    MATERIAL[2] * intensity + spec,
                ];
            }
        }
    }
}

/// A view-space vertex; `i` is the shading intensity carried through clipping.
#[derive(Clone, Copy, Default)]
struct Vertex {
    /// x right, y up, z forward.
    p: [f32; 3],
    i: f32,
}

/// A projected vertex.
#[derive(Clone, Copy)]
struct ScreenVertex {
    x: f32,
    y: f32,
    /// `1/z`, linear in screen space and larger when nearer.
    inv_z: f32,
    i: f32,
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
            };
            count += 1;
        }
    }
    count
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
    use crate::media::mesh::load_obj;

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

    #[test]
    fn frame_has_the_requested_size_and_is_opaque() {
        let frame = render(&cube(), &Camera::default(), 96, 64, 1);
        assert_eq!(frame.width, 96);
        assert_eq!(frame.height, 64);
        assert_eq!(frame.bgra.len(), 96 * 64 * 4);
        assert!(frame.bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn a_cube_covers_part_of_the_frame() {
        let frame = render(&cube(), &Camera::default(), 96, 96, 1);
        let lit = lit_pixels(&frame);
        assert!(lit > 200, "expected a visible model, got {lit} pixels");
        assert!(lit < 96 * 96, "the model should not fill the frame");
        assert!(!frame.is_empty_of_geometry());
    }

    #[test]
    fn zooming_in_covers_more_pixels() {
        let mesh = cube();
        let mut camera = Camera::default();
        let wide = lit_pixels(&render(&mesh, &camera, 96, 96, 1));
        camera.zoom_by(0.5);
        let close = lit_pixels(&render(&mesh, &camera, 96, 96, 1));
        assert!(close > wide, "close={close} wide={wide}");
    }

    #[test]
    fn supersampling_keeps_the_output_size() {
        let frame = render(&cube(), &Camera::default(), 64, 48, 2);
        assert_eq!((frame.width, frame.height), (64, 48));
        assert_eq!(frame.bgra.len(), 64 * 48 * 4);
    }

    #[test]
    fn the_nearer_triangle_wins_the_depth_test() {
        let backdrop = render(&occluded_quad(false), &Camera::default(), 128, 128, 1);
        let occluded = render(&occluded_quad(true), &Camera::default(), 128, 128, 1);
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
        let frame = render(&Mesh::default(), &Camera::default(), 64, 64, 1);
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
            camera.zoom_by(0.5);
        }
        assert_eq!(camera.zoom, MIN_ZOOM);
        for _ in 0..50 {
            camera.zoom_by(2.0);
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
        let tiny = render(&cube(), &Camera::default(), 1, 1, 1);
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
            (0.0f32, 0.0f32, 1.0f32),
            (0.62, 0.34, 1.0),
            (-1.3, -0.8, 0.5),
            (2.4, 0.4, 2.0),
            (0.0, 1.4, 1.0),
        ];
        for (yaw, pitch, zoom) in viewpoints {
            camera.yaw = yaw;
            camera.pitch = pitch;
            camera.zoom = zoom;
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
        crate::media::mesh::load_ply(ply.as_bytes()).expect("cloud parses")
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
        crate::media::mesh::load_ply(ply.as_bytes()).expect("cloud parses")
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
        let frame = render(&mesh, &Camera::default(), 64, 64, 1);
        assert!(painted(&frame) > 0, "a cloud must paint something");
    }

    /// One point is a small sprite, not a full-screen blob; the count also
    /// proves the disc test is not off by a pixel row.
    #[test]
    fn one_point_paints_a_small_sprite() {
        let mesh = cloud(&[[0.0, 0.0, 0.0]]);
        let frame = render(&mesh, &Camera::default(), 64, 64, 1);
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
        };
        let mesh = cloud(&[[0.0, 0.0, 1.0], [0.0, 0.0, -1.0]]);
        let frame = render(&mesh, &axis_on, 64, 64, 1);
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

        let frame = render(&mesh, &Camera::default(), 64, 64, 1);
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
        assert!(painted(&render(&mesh, &Camera::default(), 64, 64, 1)) > 0);
    }
}
