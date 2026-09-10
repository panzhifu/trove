// Shading for the 3D model viewport.
//
// Deliberately a mirror of the CPU rasterizer in `trove-core::media::render3d`:
// every constant arrives through the uniform block rather than being baked in
// here, so a model lit on the GPU matches the same model's thumbnail, which was
// rendered on the CPU at import time.

struct Uniforms {
    // Model space -> clip space, column-major.
    view_proj: mat4x4<f32>,
    // xyz = key light direction (unit), w unused.
    light: vec4<f32>,
    // rgb = base colour, w = ambient.
    material: vec4<f32>,
    // x = diffuse, y = specular, z = shininess, w = vignette.
    params: vec4<f32>,
    // xyz = camera position in model space, where the normals live.
    eye: vec4<f32>,
    // xy = viewport size in pixels.
    viewport: vec4<f32>,
    // rgb = gradient top.
    bg_top: vec4<f32>,
    // rgb = gradient bottom.
    bg_bottom: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct ModelOut {
    @builtin(position) clip: vec4<f32>,
    // Kept in model space so the fragment stage can shade where the normals
    // are defined, without transforming them per vertex.
    @location(0) model_pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
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
    return out;
}

@fragment
fn fs_model(in: ModelOut) -> @location(0) vec4<f32> {
    let geometric = normalize(in.normal);
    let to_eye = normalize(u.eye.xyz - in.model_pos);
    // Two-sided, so an open shell never shows black back faces.
    let n = select(-geometric, geometric, dot(geometric, to_eye) >= 0.0);

    let diffuse = max(dot(n, u.light.xyz), 0.0);
    let intensity = u.material.w + u.params.x * diffuse;
    let half = normalize(u.light.xyz + to_eye);
    let spec = u.params.y * pow(max(dot(n, half), 0.0), u.params.z);

    return vec4<f32>(u.material.rgb * intensity + vec3<f32>(spec), 1.0);
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
