// Shading for the 3D model viewport.
//
// Deliberately a mirror of the CPU rasterizer in `trove-core::media::render3d`:
// every constant arrives through the uniform block rather than being baked in
// here, so a model lit on the GPU matches the same model's thumbnail, which was
// rendered on the CPU at import time.

// How many stops a colour scale may carry: `height_color::RAMP_STOPS`, which the
// uniform block is sized from. The test that compares the two struct sizes
// catches a drift; 32 is what CloudCompare's 23-anchor ASPRS palette needs.
const RAMP_STOPS: u32 = 32u;

struct Uniforms {
    // Model space -> clip space, column-major.
    view_proj: mat4x4<f32>,
    // xyz = key light direction (unit), w unused.
    light: vec4<f32>,
    // rgb = base colour, w = ambient.
    material: vec4<f32>,
    // x = diffuse, y = specular, z = shininess, w = vignette.
    params: vec4<f32>,
    // x = point sprite radius in pixels, y = eye-dome lighting strength,
    // z/w = the depth-to-log-depth constants the point-cloud post pass reads.
    params2: vec4<f32>,
    // xyz = camera position in model space, where the normals live.
    eye: vec4<f32>,
    // xy = viewport size in pixels.
    viewport: vec4<f32>,
    // rgb = gradient top.
    bg_top: vec4<f32>,
    // rgb = gradient bottom.
    bg_bottom: vec4<f32>,
    // x = the height mode (0 none, 1 colour scale, 2 bands), y = which axis is
    // the height (0=X, 1=Y, 2=Z), z = the range floor, w = 1 / the range span.
    coloring: vec4<f32>,
    // x = banding radians per field unit, y = how many `ramp` entries are
    // live, z = which field is being read (0 height, 1 slope, 2 aspect,
    // 3 intensity, 4 class), w = whether the scale's anchors are bins.
    coloring_params: vec4<f32>,
    // The active colour scale: rgb = colour, w = position along the scale,
    // ascending, zero-padded past the live count.
    ramp: array<vec4<f32>, RAMP_STOPS>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

// How far behind the red channel the green and blue ones start, matching
// `height_color::BAND_PHASE_2` and `_3`.
const BAND_PHASE_2: f32 = 2.0944;
const BAND_PHASE_3: f32 = 4.1888;

// One component of a vector by the chosen axis: 0 = x, 1 = y, 2 = z. The index
// is a runtime value, so this is a switch rather than a subscript, and it is the
// same switch the CPU makes in `height_color::component`.
fn component(v: vec3<f32>, axis: u32) -> f32 {
    switch (axis) {
        case 0u: { return v.x; }
        case 1u: { return v.y; }
        default: { return v.z; }
    }
}

// The two components that are not the chosen axis, in the fixed order
// `height_color::horizontal` documents. A lean's bearing is `atan2` of these
// two, so the two renderers have to agree on which is which.
fn horizontal(v: vec3<f32>, axis: u32) -> vec2<f32> {
    switch (axis) {
        case 0u: { return vec2<f32>(v.y, v.z); }
        case 1u: { return vec2<f32>(v.x, v.z); }
        default: { return vec2<f32>(v.x, v.y); }
    }
}

// How far a normal leans away from the chosen axis, in degrees: 0 flat along it,
// 90 across. Folded with `abs`, because facing down the axis is as flat as
// facing up — which is what a dip means.
fn dip_degrees(n: vec3<f32>, axis: u32) -> f32 {
    let length = length(n);
    if length < 1e-6 {
        return 0.0;
    }
    return degrees(acos(clamp(abs(component(n, axis)) / length, 0.0, 1.0)));
}

// The bearing of that lean, folded into one turn the same way the CPU's
// `rem_euclid` is.
fn aspect_degrees(n: vec3<f32>, axis: u32) -> f32 {
    let lean = horizontal(n, axis);
    return fract(degrees(atan2(lean.x, lean.y)) / 360.0) * 360.0;
}

// The value the colour runs along, for whichever field the look reads.
//
// `n` is the geometry's own normal from the vertex attribute, not the one the
// fragment stage flips to face the camera: dip direction reads a 180 degree
// difference out of it. `intensity` and `class_id` come off the same instance
// the colour does, and a mesh — which has no such attributes — never asks for
// them: the viewport will not offer a field whose channel the model does not
// carry.
fn field_value(p: vec3<f32>, n: vec3<f32>, intensity: f32, class_id: f32) -> f32 {
    let axis = u32(u.coloring.y + 0.5);
    switch (u32(u.coloring_params.z + 0.5)) {
        case 1u: { return dip_degrees(n, axis); }
        case 2u: { return aspect_degrees(n, axis); }
        case 3u: { return intensity; }
        case 4u: { return class_id; }
        default: { return component(p, axis); }
    }
}

// The colour the scale gives at position `t`, interpolating between the stops
// around it and clamped outside the ends. Mirrors `height_color::Ramp::color_at`,
// which is itself `ccColorScale`'s interval walk over its resampled stops.
fn ramp_color(t: f32) -> vec3<f32> {
    let count = u32(u.coloring_params.y);
    if count < 2u {
        // Nothing to interpolate between: the CPU paints an invalid scale
        // black, and so does this.
        return vec3<f32>(0.0);
    }
    let x = clamp(t, 0.0, 1.0);
    if u.coloring_params.w > 0.5 {
        // A classification scale: the value names a bin, and a bin's colour is
        // its own. `height_color::Ramp::color_at` takes the same branch.
        let bin = min(u32(x * f32(count)), count - 1u);
        return u.ramp[bin].rgb;
    }
    var interval = 0u;
    while interval + 2u < count && u.ramp[interval + 1u].w < x {
        interval = interval + 1u;
    }
    let before = u.ramp[interval];
    let after = u.ramp[interval + 1u];
    let span = after.w - before.w;
    let alpha = select(0.0, (x - before.w) / span, span > 0.0);
    return mix(before.rgb, after.rgb, alpha);
}

// The colour one band of the stripe cycle is painted with: three sines a third
// of a cycle apart, whose sum — and so whose brightness — stays constant.
// CloudCompare's `setRGBColorByBanding`, with `coloring_params.x` as its
// `bands` term. The value is the raw one rather than a distance from the range
// floor, which is what makes a cycle a known distance to read off.
fn band_color(value: f32) -> vec3<f32> {
    let z = u.coloring_params.x * value;
    return vec3<f32>(
        sin(z) * 0.5 + 0.5,
        sin(z + BAND_PHASE_2) * 0.5 + 0.5,
        sin(z + BAND_PHASE_3) * 0.5 + 0.5,
    );
}

// The base colour for a model-space point: the file's own colour, or what the
// field look asks for.
//
// Resolved here, from the model-space position and normal, so changing the look
// costs a uniform write rather than a vertex-buffer re-upload — which is what
// makes it affordable on a streamed cloud of tens of millions of points.
fn surface_color(
    p: vec3<f32>,
    n: vec3<f32>,
    intensity: f32,
    class_id: f32,
    own: vec3<f32>,
) -> vec3<f32> {
    let value = field_value(p, n, intensity, class_id);
    switch (u32(u.coloring.x + 0.5)) {
        case 1u: { return ramp_color((value - u.coloring.z) * u.coloring.w); }
        case 2u: { return band_color(value); }
        default: { return own; }
    }
}

struct ModelOut {
    @builtin(position) clip: vec4<f32>,
    // Kept in model space so the fragment stage can shade where the normals
    // are defined, without transforming them per vertex.
    @location(0) model_pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    // Base colour: the material, or what the height look asks for. Resolved
    // here, from the model-space position, so toggling the look costs a uniform
    // write rather than a vertex-buffer re-upload.
    @location(2) tint: vec3<f32>,
};

@vertex
fn vs_model(
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
) -> ModelOut {
    var out: ModelOut;
    out.clip = u.view_proj * vec4<f32>(position, 1.0);
    out.model_pos = position;
    out.normal = normal;
    // A triangle has no scalar channels to read: the attributes stop at the
    // normal, and the two the point path carries are zero here by construction.
    out.tint = surface_color(position, normal, 0.0, 0.0, u.material.rgb);
    return out;
}

@fragment
fn fs_model(in: ModelOut) -> @location(0) vec4<f32> {
    let geometric = normalize(in.normal);
    let to_eye = normalize(u.eye.xyz - in.model_pos);
    // Two-sided, so an open shell never shows black back faces.
    let n = select(-geometric, geometric, dot(geometric, to_eye) >= 0.0);

    let diffuse = max(dot(n, u.light.xyz), 0.0);
    let shading = u.material.w + u.params.x * diffuse;
    let half = normalize(u.light.xyz + to_eye);
    let spec = u.params.y * pow(max(dot(n, half), 0.0), u.params.z);

    return vec4<f32>(in.tint * shading + vec3<f32>(spec), 1.0);
}

// A point cloud is drawn one sprite per point, each a camera-facing square
// that the fragment stage clips into a disc. The corners come from the vertex
// index and the point itself from an instance buffer, so a cloud needs no
// per-vertex geometry on the host.
struct PointOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) model_pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    // Unit square coordinate: -1..1 across the sprite, for the disc test.
    @location(2) offset: vec2<f32>,
    // The point's own colour; the host substitutes the material when the file
    // carries none, so the two renderers cannot disagree.
    @location(3) color: vec3<f32>,
};

@vertex
fn vs_point(
    @builtin(vertex_index) index: u32,
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) intensity: f32,
    @location(4) class_id: f32,
) -> PointOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
    );
    let corner = corners[index % 6u];

    let base = u.view_proj * vec4<f32>(position, 1.0);
    // Half the viewport in pixels maps to one unit of clip space, so the
    // sprite keeps its pixel size at any depth. Scaling by `w` cancels the
    // perspective divide the rasterizer is about to do.
    let radius = u.params2.x;
    let extent = vec2<f32>(
        radius * 2.0 / max(u.viewport.x, 1.0),
        radius * 2.0 / max(u.viewport.y, 1.0),
    );

    var out: PointOut;
    out.clip = vec4<f32>(base.xy + corner * extent * base.w, base.z, base.w);
    out.model_pos = position;
    out.normal = normal;
    out.offset = corner;
    out.color = surface_color(position, normal, intensity, class_id, color);
    return out;
}

@fragment
fn fs_point(in: PointOut) -> @location(0) vec4<f32> {
    // Square sprite, round point.
    if dot(in.offset, in.offset) > 1.0 {
        discard;
    }

    // Same lighting as a triangle, using the normal the host supplied: the
    // file's own when it has one, otherwise the direction the point sits in
    // relative to the model centre.
    let geometric = normalize(in.normal);
    let to_eye = normalize(u.eye.xyz - in.model_pos);
    let n = select(-geometric, geometric, dot(geometric, to_eye) >= 0.0);

    let diffuse = max(dot(n, u.light.xyz), 0.0);
    let shading = u.material.w + u.params.x * diffuse;

    return vec4<f32>(in.color * shading, 1.0);
}

// A single oversized triangle covering the viewport, so the backdrop gets the
// same vertical gradient and corner vignette the CPU renderer paints.
@vertex
fn vs_backdrop(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let corner = corners[index];
    return vec4<f32>(corner, 0.0, 1.0);
}

@fragment
fn fs_backdrop(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = max(u.viewport.xy, vec2<f32>(1.0, 1.0));
    var color = mix(u.bg_top.rgb, u.bg_bottom.rgb, position.y / size.y);

    let centered = vec2<f32>(position.x / size.x, position.y / size.y) * 2.0 - vec2<f32>(1.0, 1.0);
    let vignette = min(dot(centered, centered) * 0.5, 1.0) * u.params.w;
    color = mix(color, u.bg_bottom.rgb, vignette);

    return vec4<f32>(color, 1.0);
}

// Eye-dome lighting and gap filling, over the depth the point pass left.
//
// This is the GPU half of `render3d::enhance_points`: the same algorithm
// reading the same depth, so a cloud lit here matches one lit on the CPU. It
// runs after the points as a full-screen pass, and only for a settled
// point-cloud frame — a draft is drawn without MSAA and stretched back over
// the canvas, and the creases it would compute could not survive the scaling.
//
// The depth binding is the multisampled depth attachment itself (sample 0),
// which is why this pass carries a bind group of its own and why it needs an
// adapter that multisamples: without MSAA there is no such texture to read,
// and the frame is drawn without the effect rather than with a broken one.
@group(1) @binding(0) var edl_depth: texture_depth_multisampled_2d;
@group(1) @binding(1) var edl_source: texture_2d<f32>;

fn drew(coord: vec2<i32>) -> bool {
    return textureLoad(edl_depth, coord, 0) < 1.0;
}

// The CPU keeps `-log2(1/z)`, which is `log2(w)` for view depth `w`. Depth is
// affine in `1/w` (`ndc = z_scale + z_bias / w`), so the same value comes back
// from the depth buffer with the two constants the host packs into `params2`.
// Subtracting two of them gives the log of the ratio of their distances, in
// the right order.
fn log_depth_at(coord: vec2<i32>) -> f32 {
    let ndc = textureLoad(edl_depth, coord, 0);
    if ndc >= 1.0 {
        return 0.0;
    }
    return log2(max(u.params2.w / (ndc - u.params2.z), 1e-6));
}

// Whether the 3×3 neighbour at an offset drew something. Off the edge counts
// as not drawn, which is what the CPU's `has_left`/`has_up` guards do.
fn occupies(coord: vec2<i32>, size: vec2<i32>, offset: vec2<i32>) -> bool {
    let p = coord + offset;
    if p.x < 0 || p.y < 0 || p.x >= size.x || p.y >= size.y {
        return false;
    }
    return drew(p);
}

fn flag(value: bool) -> u32 {
    return select(0u, 1u, value);
}

@fragment
fn fs_edl(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = vec2<i32>(u.viewport.xy);
    let coord = clamp(vec2<i32>(position.xy), vec2<i32>(0), size - vec2<i32>(1));
    let color = textureLoad(edl_source, coord, 0).rgb;

    // A pixel nothing drew is a gap between the discs, and takes the average
    // of the colours around it — but only where the neighbourhood supports it:
    // filling next to a silhouette would grow the model outwards into the
    // background by a pixel. The domino rule is Nimbus's, kept as the CPU
    // keeps it.
    if !drew(coord) {
        let x0 = max(coord.x - 1, 0);
        let x1 = min(coord.x + 1, size.x - 1);
        let y0 = max(coord.y - 1, 0);
        let y1 = min(coord.y + 1, size.y - 1);
        let lt = occupies(coord, size, vec2<i32>(-1, -1));
        let mt = occupies(coord, size, vec2<i32>(0, -1));
        let rt = occupies(coord, size, vec2<i32>(1, -1));
        let lm = occupies(coord, size, vec2<i32>(-1, 0));
        let rm = occupies(coord, size, vec2<i32>(1, 0));
        let lb = occupies(coord, size, vec2<i32>(-1, 1));
        let mb = occupies(coord, size, vec2<i32>(0, 1));
        let rb = occupies(coord, size, vec2<i32>(1, 1));

        let neighbours = flag(lt) + flag(mt) + flag(rt) + flag(lm)
            + flag(rm) + flag(lb) + flag(mb) + flag(rb);
        let window = u32((x1 - x0 + 1) * (y1 - y0 + 1) - 1);
        let every_direction = (lt || mt || rt || rm || mb)
            && (lt || mt || rt || lm || rm)
            && (lt || mt || lm || lb || mb)
            && (lm || rm || lb || mb || rb)
            && (lt || mt || rt || rm || rb)
            && (lt || mt || rt || lm || lb)
            && (lt || lm || lb || mb || rb)
            && (rt || rm || rb || mb || lb);
        if neighbours == window || every_direction {
            var sum = vec3<f32>(0.0);
            var count = 0.0;
            for (var y = y0; y <= y1; y += 1) {
                for (var x = x0; x <= x1; x += 1) {
                    let p = vec2<i32>(x, y);
                    if drew(p) {
                        sum += textureLoad(edl_source, p, 0).rgb;
                        count += 1.0;
                    }
                }
            }
            if count > 0.0 {
                return vec4<f32>(sum / count, 1.0);
            }
        }
        // Nothing to fill from: the backdrop pass's own colour stands.
        return vec4<f32>(color, 1.0);
    }

    // Eye-dome lighting: average how much farther each drawn neighbour is, and
    // darken by that. It only ever darkens, so the background is untouched.
    let center = log_depth_at(coord);
    var sum = 0.0;
    var count = 0.0;
    let steps = array<vec2<i32>, 4>(
        vec2<i32>(-1, 0),
        vec2<i32>(1, 0),
        vec2<i32>(0, -1),
        vec2<i32>(0, 1),
    );
    for (var i = 0; i < 4; i += 1) {
        let p = coord + steps[i];
        if p.x < 0 || p.y < 0 || p.x >= size.x || p.y >= size.y {
            continue;
        }
        if drew(p) {
            sum += max(center - log_depth_at(p), 0.0);
            count += 1.0;
        }
    }
    var factor = 1.0;
    if count > 0.0 {
        factor = exp(-(sum / count) * u.params2.y);
    }
    return vec4<f32>(color * factor, 1.0);
}
