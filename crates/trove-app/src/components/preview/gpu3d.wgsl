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

// One studio light, as the host packs it: direction plus wrap, then the
// light's diffuse and specular colours.
struct LightUniform {
    // xyz = direction toward the light, in model space (unit); w = wrap.
    dir_wrap: vec4<f32>,
    // rgb = the light's diffuse colour.
    diffuse: vec4<f32>,
    // rgb = the light's specular colour.
    specular: vec4<f32>,
};

struct Uniforms {
    // Model space -> clip space, column-major.
    view_proj: mat4x4<f32>,
    // Model space -> the key light's shadow map, column-major: NDC in xy (the
    // fragment folds it to the map's uv), reversed depth in z — nearer to the
    // light is larger, exactly the main pass's convention.
    view_proj_shadow: mat4x4<f32>,
    // rgb = the flat material colour; w unused.
    material: vec4<f32>,
    // x = material roughness, y/w unused, z = vignette strength.
    params: vec4<f32>,
    // The four studio lights, already turned into model space for this
    // frame's camera.
    lights: array<LightUniform, 4>,
    // The camera basis in model space — right, up, forward. The environment
    // cubemap is baked in view space (the rig is camera-anchored), so a
    // reflection direction folds back through this basis before sampling.
    basis: array<vec4<f32>, 3>,
    // The studio environment's first two harmonic bands: L0.M0, L1.Mn1,
    // L1.M0, L1.Mp1, rgb each — the term the roughest reflections mix into.
    env_sh: array<vec4<f32>, 4>,
    // x = point sprite radius in pixels, y = eye-dome lighting strength,
    // z/w = the depth-to-log-depth constants the point-cloud post pass reads.
    // w is also one over the shadow map's edge in texels — the PCF taps' uv
    // stride and the slope bias's scale.
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
// The model's base-colour textures as one array, layer 0 white: a vertex
// whose primitive has no texture points here, and "multiply by white" is
// "do nothing". sRGB format, so the sample arrives linear like everything
// else the shading multiplies.
@group(0) @binding(1) var model_textures: texture_2d_array<f32>;
@group(0) @binding(2) var model_sampler: sampler;
// The prefiltered studio environment: five mips from the sharp room (mip 0)
// to the roughness-0.7 blur (mip 4), baked once on the host — EEVEE's
// material-preview studio light as a cubemap instead of an HDRI file.
@group(0) @binding(3) var env_tex: texture_cube<f32>;
// The key light's shadow map, baked by this frame's depth-only pass, and the
// comparison sampler that turns its 3×3 taps into a PCF. A comparison
// against GreaterEqual answers 1 where the reference depth is at or in front
// of the stored one — "lit" — which is the direction reversed-Z wants.
@group(0) @binding(4) var shadow_map: texture_depth_2d;
@group(0) @binding(5) var shadow_cmp: sampler_comparison;
// The split-sum environment BRDF table the host bakes once: u = NoV, v =
// linear roughness, rg = (scale, bias) the environment's radiance weights
// the surface's F0 with. Clamped linear sampling, which is what the CPU's
// own bilinear read of the same table reproduces.
@group(0) @binding(6) var brdf_lut: texture_2d<f32>;
@group(0) @binding(7) var lut_sampler: sampler;

// Which studio light casts shadows: the key light, `render3d::KEY_LIGHT` —
// slot 1 of the rig the host packs, the only strong one. The three fills
// would cost three more shadow passes to darken a shadow by a shade.
const KEY_LIGHT: u32 = 1u;

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
    // The vertex's own base colour — the material, or a vertex colour — left
    // un-resolved. The fragment stage folds the height look into it, from the
    // normal it resolves there, so a mesh whose normal only exists in the
    // fragment stage (one the vertex stage left zero) is looked up correctly.
    @location(2) tint: vec3<f32>,
    // u, v, the base-colour layer and the metallic-roughness layer the
    // vertex samples; layers 0 is the white stand-in.
    @location(3) tex_meta: vec4<f32>,
    // x = metallic factor, y = roughness factor, z = the normal-map layer,
    // w = the ambient-occlusion layer. Layer 0 answers white, which the
    // occlusion folds away on its own; the normal map is selected off
    // explicitly, a white tangent-space normal naming no direction.
    @location(4) mat_meta: vec4<f32>,
    // x = the normal map's scale, y = the occlusion strength.
    @location(5) mat_scalars: vec2<f32>,
    // x = the emissive layer (0 = none), y/z/w = the emissive factor.
    @location(6) emissive_meta: vec4<f32>,
    // x = the alpha cutoff a texel's coverage is tested against (negative =
    // opaque, nothing is ever discarded), y = the base-colour alpha factor
    // that multiplies the texel into the value tested, z = the material's
    // doubleSided flag (1 = its back faces are surfaces and are drawn).
    @location(7) alpha_meta: vec4<f32>,
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
    // The vertex's own base colour, un-resolved: the height look needs the
    // surface normal, which a mesh without one only has in the fragment stage,
    // so `fs_model` computes the tint from the normal it resolves there.
    out.tint = u.material.rgb;
    out.mat_meta = vec4<f32>(0.0);
    out.mat_scalars = vec2<f32>(0.0);
    out.emissive_meta = vec4<f32>(0.0);
    // The plain pipeline has no texture to read an alpha from and no
    // material to own a doubleSided flag, so the tests never fire: negative
    // cutoff, a factor nothing multiplies, and single-sided by geometry —
    // the pipeline's own culling already answered that.
    out.alpha_meta = vec4<f32>(-1.0, 1.0, 0.0, 0.0);
    return out;
}

// The same vertex stage for a mesh that carries its own colours — a glTF
// primitive's base colour factor, an OBJ material's diffuse. One extra vertex
// buffer of RGB, and otherwise the identical path: the flat material the plain
// pipeline takes from the uniform block is per-vertex here.
@vertex
fn vs_model_colored(
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) tex_meta: vec4<f32>,
    @location(4) mat_meta: vec4<f32>,
    @location(5) mat_scalars: vec2<f32>,
    @location(6) emissive_meta: vec4<f32>,
    @location(7) alpha_meta: vec4<f32>,
) -> ModelOut {
    var out: ModelOut;
    out.clip = u.view_proj * vec4<f32>(position, 1.0);
    out.model_pos = position;
    out.normal = normal;
    // The vertex's own base colour, un-resolved — see `vs_model`.
    out.tint = color;
    out.tex_meta = tex_meta;
    out.mat_meta = mat_meta;
    out.mat_scalars = mat_scalars;
    out.emissive_meta = emissive_meta;
    out.alpha_meta = alpha_meta;
    return out;
}

// Blender workbench's `wrapped_lighting`: a diffuse term whose terminator the
// light's wrap softens. The dot is deliberately unclamped — the wrap is what
// folds light around past the horizon.
fn wrapped_light(nl: f32, wrap: f32) -> f32 {
    let denom = (wrap + 1.0) * (wrap + 1.0);
    return clamp((nl + wrap) / denom, 0.0, 1.0);
}

// How much of the key light survives at this pixel: 1 fully lit, 0 fully
// shadowed, in between across the penumbra the taps straddle.
//
// The reference depth carries the slope-scaled bias the CPU rasteriser runs
// the same formula for: a surface tilted away from the light sinks `tan θ`
// of depth per texel (`0.625 / N` is one texel's share of the reversed
// range, with `N` the map edge arriving as `params2.w`), and the taps reach
// `√2` texels diagonally, so two texels of slope keep every tap honest
// without detaching the shadow. Grazing surfaces would need an unbounded
// `tan θ`; past 3.0 the bias stops growing, which is where peter-panning is
// least visible anyway. The 3×3 taps straddle the pixel's light-space
// position, and the comparison sampler answers each with a hardware-filtered
// compare, so the average is a soft penumbra at nine taps' cost.
fn shadow_factor(model_pos: vec3<f32>, n: vec3<f32>, no_l: f32) -> f32 {
    let clip = u.view_proj_shadow * vec4<f32>(model_pos, 1.0);
    let slope = min(sqrt(max(1.0 - no_l * no_l, 0.0)) / max(no_l, 0.1), 3.0);
    let reference = clip.z + 0.625 * u.params2.w * 2.0 * slope;
    var sum = 0.0;
    for (var dy = -1; dy <= 1; dy++) {
        for (var dx = -1; dx <= 1; dx++) {
            // NDC xy folds to the map's uv: u = 0.5 + x/2, and — because
            // framebuffer row 0 is the top of the light's view and a render
            // attachment's row 0 is texture v = 0 — v = 0.5 - y/2, the
            // flipped way. BOTH halves of that fold need their 0.5. Dropping
            // the `+ 0.5` from u (leaving `clip.x * 0.5`, a range of -0.5..0.5)
            // put every lookup in the map's left half and clamped it to
            // column 0 at the edge, so a surface tested itself against a
            // column half a map away and never found the caster — the box at
            // (480, 512) that the readback proves is there.
            let at = vec2<f32>(0.5 + clip.x * 0.5, 0.5 - clip.y * 0.5)
                + vec2<f32>(f32(dx), f32(dy)) * u.params2.w;
            sum += textureSampleCompareLevel(shadow_map, shadow_cmp, at, reference);
        }
    }
    return sum / 9.0;
}

// The diffuse half of the studio lighting, summed over the four lights: a
// colour-free light sum a surface colour multiplies. Mirrors
// `get_world_lighting`'s diffuse loop in Blender's workbench. The key light's
// term carries the pixel's shadow factor — `key_shadow` is 1 wherever the
// caller does not test (the points, an off switch), so the fills and the
// environment are never touched by it and a shadow keeps a quarter of the
// light instead of going pitch black.
fn studio_diffuse(n: vec3<f32>, key_shadow: f32) -> vec3<f32> {
    var sum = vec3<f32>(0.0);
    for (var i = 0u; i < 4u; i++) {
        let light = u.lights[i];
        var lit = wrapped_light(dot(light.dir_wrap.xyz, n), light.dir_wrap.w);
        lit = select(lit, lit * key_shadow, i == KEY_LIGHT);
        sum += lit * light.diffuse.xyz;
    }
    return sum;
}

// The specular half: the normalized-Blinn term each light contributes,
// plus the studio environment the reflection direction sees. Mirrors the
// CPU's `studio_specular_parts` in `render3d`. The key light's highlight
// dims with its shadow — a highlight has no business surviving inside the
// shadow of the light that casts it; the environment's reflection is the
// room's, not the key's, and stands.
//
// The two halves wear different colours. The direct lights take the
// Fresnel-lifted specular colour, Blender's fast path — its luminance is
// also what the diffuse gives back, the one-knob energy conservation,
// returned in the vec4's alpha. The environment takes the split-sum
// weighting instead: `F0 · scale + bias` off the baked BRDF table at this
// incidence and roughness, EEVEE's lightprobe eval — the grazing response a
// one-lift approximation loses is exactly what the environment reflects.
fn studio_specular(
    n: vec3<f32>,
    to_eye: vec3<f32>,
    roughness: f32,
    key_shadow: f32,
    metallic: f32,
    albedo: vec3<f32>,
) -> vec4<f32> {
    let spec_f0 = mix(vec3<f32>(0.05), albedo, vec3<f32>(metallic));
    let nv = clamp(dot(n, to_eye), 0.0, 1.0);
    let fresnel = exp2(-8.35 * nv) * (1.0 - roughness);
    let spec_color = mix(spec_f0, vec3<f32>(1.0), vec3<f32>(fresnel));
    var sum = vec3<f32>(0.0);
    for (var i = 0u; i < 4u; i++) {
        let light = u.lights[i];
        let half = normalize(light.dir_wrap.xyz + to_eye);
        let spec_angle = clamp(dot(half, n), 0.0, 1.0);
        let nl = clamp(dot(light.dir_wrap.xyz, n), 0.0, 1.0);
        // A wrapped light is a bigger, softer light: its gloss shrinks and
        // its highlight widens accordingly.
        let gloss = (1.0 - roughness) * (1.0 - light.dir_wrap.w);
        let shininess = exp2(10.0 * gloss + 1.0);
        var s = pow(spec_angle, shininess) * nl * (shininess * 0.125 + 1.0);
        s = select(s, s * key_shadow, i == KEY_LIGHT);
        sum += s * light.specular.xyz;
    }
    // The direct lights wear the lifted colour...
    sum *= spec_color;
    // The environment joins them: the prefiltered studio cubemap along the
    // reflection direction, mixed toward the harmonic irradiance as the
    // roughness passes 0.7 — EEVEE's lightprobe eval (`roughness_to_lod` /
    // `roughness_to_mix_fac` / the SH Lambert term are its
    // `eevee_lightprobe_sphere.bsl.hh` verbatim) — weighted by the split-sum
    // pair. The cubemap is baked in view space, so the reflection folds back
    // through the camera basis.
    let mirror = 2.0 * dot(n, to_eye) * n - to_eye;
    let dir_view = vec3<f32>(
        dot(mirror, u.basis[0].xyz),
        dot(mirror, u.basis[1].xyz),
        dot(mirror, u.basis[2].xyz),
    );
    let lod = roughness_to_lod(roughness);
    let fac = roughness_to_mix_fac(roughness);
    let radiance = mix(
        textureSampleLevel(env_tex, model_sampler, dir_view, lod).rgb,
        sh_lambert(dir_view),
        fac,
    );
    let brdf = textureSampleLevel(brdf_lut, lut_sampler, vec2<f32>(nv, roughness), 0.0).rg;
    sum += radiance * (spec_f0 * brdf.x + brdf.y);
    return vec4<f32>(sum, dot(spec_color, vec3<f32>(1.0 / 3.0)));
}

// Linear roughness → prefiltered mip: Frostbite's eq. 53 with the 0.4 linear
// mix, over EEVEE's five-level chain capped at roughness 0.7. Mirrors the
// CPU's `lod_to_roughness`.
fn roughness_to_lod(roughness: f32) -> f32 {
    let ratio = clamp(roughness / 0.7, 0.0, 1.0);
    let mip_ratio = mix(ratio, sqrt(ratio), 0.4);
    return mip_ratio * 4.0;
}

// Past roughness 0.7 the reflection dissolves into the environment's
// irradiance: EEVEE's `roughness_to_mix_fac`, squared so the crossover is
// gentle. Mirrors the CPU-side shape.
fn roughness_to_mix_fac(roughness: f32) -> f32 {
    return pow(clamp((roughness - 0.7) / 0.2, 0.0, 1.0), 2.0);
}

// The environment's lambert irradiance from the first two harmonic bands —
// EEVEE's `SphericalHarmonicL1::evaluate_lambert`: L0 plus L1 weighted 2/3,
// clamped out of the negative lobes. The basis constants are Blender's.
fn sh_lambert(dir_view: vec3<f32>) -> vec3<f32> {
    let l0 = u.env_sh[0].rgb * 0.282094792;
    let l1 = u.env_sh[1].rgb * (-0.488602512 * dir_view.y)
        + u.env_sh[2].rgb * (0.488602512 * dir_view.z)
        + u.env_sh[3].rgb * (-0.488602512 * dir_view.x);
    return max(l0 + l1 * (2.0 / 3.0), vec3<f32>(0.0));
}

// The scene-linear → sRGB display transform. Blender shades in scene-linear
// and converts on the way to the screen — its studio-light numbers are linear
// intensities, and so are a glTF file's colours — so the shading result is
// encoded where the shading is written. Mirrors the CPU's `encode_channel`.
fn encode(c: vec3<f32>) -> vec3<f32> {
    let low = c * 12.92;
    let high = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(high, low, c <= vec3<f32>(0.0031308));
}

// Scene-linear sRGB → display code values: Blender's default AgX view
// transform, in the compact form three.js fits to Blender's own AgX LUTs —
// into Rec.2020, the inset matrix, a log2 encode that the contrast spline
// shapes, and back out — then the sRGB encode the screen wants. This is what
// makes the highlights roll off and the saturated colours hold their hue the
// way Blender's viewport shows them. Only the *final* pixel goes through
// here: the metallic-roughness and normal-map channels are stored data, not
// display colour, and keep the bare `encode`. Mirrors the CPU's
// `display_color`.
fn display(color: vec3<f32>) -> vec3<f32> {
    const srgb_to_rec2020 = mat3x3<f32>(
        vec3<f32>(0.6274039, 0.0690973, 0.0163914),
        vec3<f32>(0.329283, 0.9195404, 0.0880133),
        vec3<f32>(0.0433131, 0.0113623, 0.8955953),
    );
    const rec2020_to_srgb = mat3x3<f32>(
        vec3<f32>(1.660491, -0.1245505, -0.0181508),
        vec3<f32>(-0.5876411, 1.1328999, -0.1005789),
        vec3<f32>(-0.0728499, -0.0083494, 1.1187297),
    );
    // Column-major like every matrix here: column 0 is the CPU row-form's
    // first column, so `agx_inset * c` multiplies the colour the way
    // `display_color`'s row multiplication does. (Copying the rows in as
    // columns silently transposes the pair, and every neutral grey the
    // transform is meant to hold picks up a blue-violet cast.)
    const agx_inset = mat3x3<f32>(
        vec3<f32>(0.856627153315983, 0.137318972929847, 0.11189821299995),
        vec3<f32>(0.0951212405381588, 0.761241990602591, 0.0767994186031903),
        vec3<f32>(0.0482516061458583, 0.101439036467562, 0.811302368396859),
    );
    const agx_outset = mat3x3<f32>(
        vec3<f32>(1.1271005818144368, -0.1413297634984383, -0.14132976349843826),
        vec3<f32>(-0.11060664309660323, 1.157823702216272, -0.11060664309660294),
        vec3<f32>(-0.016493938717834573, -0.016493938717834257, 1.2519364065950405),
    );
    // log2(2^-10 * 0.18) .. log2(2^6.5 * 0.18): the scene range AgX was fit
    // to, in stops around middle grey.
    const min_ev = -12.47393;
    const max_ev = 4.026069;

    var c = srgb_to_rec2020 * color;
    c = agx_inset * c;
    c = clamp(log2(max(c, vec3<f32>(1e-10))), vec3<f32>(min_ev), vec3<f32>(max_ev));
    c = (c - vec3<f32>(min_ev)) / vec3<f32>(max_ev - min_ev);
    // The contrast spline: a monotone curve through middle grey with the
    // shoulder and toe AgX is named for.
    let x2 = c * c;
    let x4 = x2 * x2;
    c = 15.5 * x4 * x2 - 40.14 * x4 * c + vec3<f32>(31.96) * x4 - 6.868 * x2 * c
        + 0.4298 * x2
        + 0.1191 * c
        - vec3<f32>(0.00232);
    c = agx_outset * c;
    c = pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.2));
    return encode(clamp(rec2020_to_srgb * c, vec3<f32>(0.0), vec3<f32>(1.0)));
}

// The normal a triangle's fragment shades with: the interpolated vertex
// normal, or — for a mesh that carries none, where the vertex stage wrote a
// zero — the geometric normal of the triangle, from the screen-space
// derivatives of the model position. The two-sided `select` the callers apply
// orients it toward the eye either way.
fn surface_normal(model_pos: vec3<f32>, normal: vec3<f32>) -> vec3<f32> {
    if dot(normal, normal) > 1e-12 {
        return normalize(normal);
    }
    return normalize(cross(dpdx(model_pos), dpdy(model_pos)));
}

@fragment
fn fs_model(in: ModelOut) -> @location(0) vec4<f32> {
    let geometric = surface_normal(in.model_pos, in.normal);
    let to_eye = normalize(u.eye.xyz - in.model_pos);
    // Two-sided, so an open shell never shows black back faces.
    let n = select(-geometric, geometric, dot(geometric, to_eye) >= 0.0);
    // The base colour: the material, or the height look resolved against the
    // surface normal — here, not in the vertex stage, so a mesh that carries
    // no normal of its own is looked up against the geometric normal rather
    // than the zero the vertex stage had.
    let tint = surface_color(in.model_pos, geometric, 0.0, 0.0, in.tint);

    // The key light's test runs before the light sums that read it: how much
    // of the light this pixel can see decides both its diffuse and its
    // highlight.
    let shadow = shadow_factor(
        in.model_pos,
        n,
        clamp(dot(u.lights[KEY_LIGHT].dir_wrap.xyz, n), 0.0, 1.0),
    );

    let diffuse = studio_diffuse(n, shadow);
    let spec = studio_specular(n, to_eye, u.params.x, shadow, 0.0, tint);
    let specular = spec.xyz;
    let energy = spec.w;

    return vec4<f32>(display(tint * diffuse * (1.0 - energy) + specular), 1.0);
}

// The normal map's rotation applied in the tangent frame the screen-space
// derivatives of the position and the UV describe: the tangent runs along
// increasing U and the bitangent along increasing V — the frame the glTF
// `TANGENT` attribute describes, checked vertex for vertex against a real
// export — so the map's red multiplies U's direction and its green V's, no
// flip. Mirrors the CPU's `relief_normal`. A degenerate determinant (a seam's
// zero-area UV span) keeps the geometric normal. The derivatives are taken
// before any branching, in uniform control flow as WGSL requires.
fn perturb_normal(
    n: vec3<f32>,
    p: vec3<f32>,
    uv: vec2<f32>,
    tangent_normal: vec3<f32>,
) -> vec3<f32> {
    let dp1 = dpdx(p);
    let dp2 = dpdy(p);
    let duv1 = dpdx(uv);
    let duv2 = dpdy(uv);
    let det = duv1.x * duv2.y - duv1.y * duv2.x;
    if (abs(det) < 1.0e-10) {
        return n;
    }
    let r = 1.0 / det;
    let tangent = normalize((dp1 * duv2.y - dp2 * duv1.y) * r);
    let bitangent = normalize((dp2 * duv1.x - dp1 * duv2.x) * r);
    return normalize(
        tangent * tangent_normal.x + bitangent * tangent_normal.y + n * tangent_normal.z,
    );
}

// The coloured pipeline's fragment: the file's own colours — factor, vertex
// attributes and now the base-colour texture, sampled sRGB and decoded by
// the hardware like Blender's image textures — multiply together before the
// lighting does. Layer -1 is the untextured vertex; it clamps to layer 0,
// the white stand-in, and the multiply is inert.
//
// The texture reads use `textureSample` — implicit derivatives, so the
// sampler picks its mip off how fast the UVs cross the pixel and filters
// trilinearly along the chain the upload baked — instead of pinning level 0
// and letting minified textures shimmer. The one sample that stays explicit
// is the emissive's, whose layer is only known inside a branch: WGSL
// requires uniform control flow for implicit derivatives, and a glow map's
// aliasing costs nothing next to a cutout's.
@fragment
fn fs_model_textured(in: ModelOut) -> @location(0) vec4<f32> {
    let geometric = surface_normal(in.model_pos, in.normal);
    let to_eye = normalize(u.eye.xyz - in.model_pos);
    let n = select(-geometric, geometric, dot(geometric, to_eye) >= 0.0);
    // The base colour, height look resolved here — see `fs_model`.
    let tint = surface_color(in.model_pos, geometric, 0.0, 0.0, in.tint);

    // Round, never truncate: the perspective interpolation of an exact 1.0
    // can land at 0.9999, and a truncation would fall to the white layer.
    // The base-colour texel — alpha included, it is the coverage the
    // material's alpha test consumes — then the metallic-roughness texel:
    // G carries roughness, B carries metallic, both scaled by the
    // material's factors.
    let base_layer = i32(round(clamp(in.tex_meta.z, 0.0, 255.0)));
    let mr_layer = i32(round(clamp(in.tex_meta.w, 0.0, 255.0)));
    let base_texel = textureSample(model_textures, model_sampler, in.tex_meta.xy, base_layer);
    let mr_texel = textureSample(model_textures, model_sampler, in.tex_meta.xy, mr_layer);
    // The metallic-roughness channels are LINEAR data, not colour: the
    // sRGB format decodes them on sample, so they are re-encoded back to the
    // stored value before use. A stored 0.5 must read as 0.5, or the finish
    // the author picked quietly turns to gloss.
    let mr_raw = encode(mr_texel.rgb);
    let metallic = mr_raw.b * in.mat_meta.x;
    let roughness = mr_raw.g * in.mat_meta.y;

    // The normal map and the occlusion map, sampled at the same UV through
    // the same sRGB atlas and re-encoded to their stored values the same
    // way. The white stand-in layer leaves the occlusion inert on its own —
    // white is unoccluded; the normal map is selected off explicitly, a
    // white vector naming no direction.
    let normal_layer = i32(round(clamp(in.mat_meta.z, 0.0, 255.0)));
    let normal_raw = encode(textureSample(
        model_textures,
        model_sampler,
        in.tex_meta.xy,
        normal_layer,
    ).rgb);
    let tangent_normal = select(
        vec3<f32>(0.0, 0.0, 1.0),
        (normal_raw - vec3<f32>(0.5)) * 2.0 * vec3<f32>(in.mat_scalars.x, in.mat_scalars.x, 1.0),
        normal_layer > 0,
    );
    let ao_layer = i32(round(clamp(in.mat_meta.w, 0.0, 255.0)));
    let ao_stored = encode(vec3<f32>(textureSample(
        model_textures,
        model_sampler,
        in.tex_meta.xy,
        ao_layer,
    ).r)).r;
    let occlusion = clamp(1.0 + in.mat_scalars.y * (ao_stored - 1.0), 0.0, 1.0);

    let shaded = perturb_normal(n, in.model_pos, in.tex_meta.xy, tangent_normal);
    // The alpha test the material's alphaMode folded into two scalars: the
    // base texel's coverage, times the base-colour factor's alpha, against
    // the cutoff. Below it the fragment draws nothing at all — the hole in
    // a leaf card. A negative cutoff (an opaque material) skips the test.
    //
    // Deliberately after every derivative op above: `discard` makes the
    // remaining control flow non-uniform, and the derivative builtins
    // `perturb_normal` runs require uniform control flow — the test is a
    // pure discard, so it is safe to defer, and deferring keeps the
    // validator's uniformity analysis satisfied on every backend.
    if (in.alpha_meta.x >= 0.0 && base_texel.a * in.alpha_meta.y < in.alpha_meta.x) {
        discard;
    }
    // The material's doubleSided, the same flag the CPU rasteriser reads per
    // face: a single-sided material's back face is not the surface you see
    // from behind, and the fragment test culls it. The rasterizer could not —
    // the mesh went onto the two-sided pipeline because some OTHER material
    // is double-sided, and culling is a pipeline choice, not a per-fragment
    // one. Deferred after the derivatives above for the same uniformity
    // reason the alpha test documents.
    if (in.alpha_meta.z < 0.5 && dot(geometric, to_eye) < 0.0) {
        discard;
    }
    // The key light's shadow test, with the relief normal: a bump the map
    // describes turns its shadow with it. The alpha discard above does not
    // disturb the compare sampler — `textureSampleCompareLevel` takes no
    // derivatives, which is exactly why it can follow non-uniform flow.
    let shadow = shadow_factor(
        in.model_pos,
        shaded,
        clamp(dot(u.lights[KEY_LIGHT].dir_wrap.xyz, shaded), 0.0, 1.0),
    );
    let diffuse = studio_diffuse(shaded, shadow);

    // A metal's diffuse is zero — its colour travels in the specular — and
    // its specular colour starts at the base colour instead of the
    // dielectric constant, both per the workbench material mix.
    // The metallic mix reads the MATERIAL's colour, not the texel: the
    // texture varies per pixel, and Blender's mix uses the base colour the
    // material declares.
    let spec = studio_specular(shaded, to_eye, roughness, shadow, metallic, tint);
    let specular = spec.xyz;
    let energy = spec.w;

    // The emissive adds, unlit: the map's colour times the factor, or the
    // factor alone when the primitive carries no map. The glow survives a
    // dark scene — a window at night reads through it, not through the
    // lights.
    var lit = in.emissive_meta.yzw;
    if (in.emissive_meta.x > 0.0) {
        let emissive_layer = i32(round(clamp(in.emissive_meta.x, 0.0, 255.0)));
        let emissive_texel = textureSampleLevel(
            model_textures,
            model_sampler,
            in.tex_meta.xy,
            emissive_layer,
            0.0,
        ).rgb;
        lit *= emissive_texel;
    }

    // The same assembly the CPU rasteriser makes in `Target::triangle`: the
    // file's colour, dimmed by the finish, the occlusion and the specular's
    // energy share, times the light sum the shadow already dimmed, plus the
    // (undimmed) specular and the unlit emissive. Scene-linear until the
    // display transform lands on the whole pixel, as Blender's does.
    return vec4<f32>(
        display(
            tint * base_texel.rgb * (1.0 - metallic) * occlusion * diffuse * (1.0 - energy)
                + specular
                + lit,
        ),
        1.0,
    );
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

    // The same studio rig a triangle shades with, diffuse only: a disc a few
    // pixels across has no room for a highlight to live in. The normal is the
    // one the host supplied: the file's own when it has one, otherwise the
    // direction the point sits in relative to the model centre.
    let geometric = normalize(in.normal);
    let to_eye = normalize(u.eye.xyz - in.model_pos);
    let n = select(-geometric, geometric, dot(geometric, to_eye) >= 0.0);

    return vec4<f32>(display(in.color * studio_diffuse(n, 1.0)), 1.0);
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

// The shadow pass: the mesh seen from the key light, depth only. The
// reversed-Z depth is what the main pass's fragment tests store — nearer to
// the light is larger, nothing drawn is zero — and the orthographic window
// was sized on the host to hold the whole model, so nothing here clips. No
// fragment stage: the pipeline writes depth and nothing else.
@vertex
fn vs_shadow(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return u.view_proj_shadow * vec4<f32>(position, 1.0);
}

@fragment
fn fs_backdrop(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = max(u.viewport.xy, vec2<f32>(1.0, 1.0));
    var color = mix(u.bg_top.rgb, u.bg_bottom.rgb, position.y / size.y);

    let centered = vec2<f32>(position.x / size.x, position.y / size.y) * 2.0 - vec2<f32>(1.0, 1.0);
    let vignette = min(dot(centered, centered) * 0.5, 1.0) * u.params.z;
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

// Reversed-Z: drawn pixels store a depth above zero, the clear value is the
// far end of the range.
fn drew(coord: vec2<i32>) -> bool {
    return textureLoad(edl_depth, coord, 0) > 0.0;
}

// The CPU keeps `-log2(1/z)`, which is `log2(w)` for view depth `w`. The
// reversed buffer stores `near / w`, so its own `-log2` is the CPU's value
// shifted by the constant `log2(near)` — and the eye-dome lighting only ever
// compares two of these, so the constant cancels and the raw `-log2` of the
// stored depth lands on the same values the CPU works from.
fn log_depth_at(coord: vec2<i32>) -> f32 {
    let depth = textureLoad(edl_depth, coord, 0);
    if depth <= 0.0 {
        return 0.0;
    }
    return -log2(depth);
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
