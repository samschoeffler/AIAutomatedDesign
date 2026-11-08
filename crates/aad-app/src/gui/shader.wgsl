// Shaders for the 3D viewer (see gpu.rs). Two kinds of drawing:
// - solids: triangles lit by a light near the camera, in one colour per draw;
// - lines: edges and the grid, each vertex with its own colour, tinted per draw.

struct Uniforms {
    // World (mm) to screen.
    view_proj: mat4x4<f32>,
    // Colour for solids, tint for lines. Alpha below 1 means see-through.
    color: vec4<f32>,
    // Direction towards the main light, in world space (w unused).
    light: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct SolidOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
};

@vertex
fn vs_solid(@location(0) position: vec3<f32>, @location(1) normal: vec3<f32>) -> SolidOut {
    var out: SolidOut;
    out.clip = u.view_proj * vec4<f32>(position, 1.0);
    out.normal = normal;
    return out;
}

@fragment
fn fs_solid(in: SolidOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    var n = normalize(in.normal);
    if (!front) {
        n = -n; // the inside of a see-through part
    }
    let key = max(dot(n, normalize(u.light.xyz)), 0.0);
    let sky = 0.5 + 0.5 * n.z; // a little brighter facing up, like light from above
    let shade = 0.30 + 0.60 * key + 0.15 * sky;
    return vec4<f32>(u.color.rgb * shade, u.color.a);
}

struct LineOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_line(@location(0) position: vec3<f32>, @location(1) color: vec4<f32>) -> LineOut {
    var out: LineOut;
    var clip = u.view_proj * vec4<f32>(position, 1.0);
    // Nudge lines a hair towards the camera so edges aren't hidden by their own faces.
    // With the camera's near plane at 2% of its distance this is about 0.05% of that
    // distance (0.1 mm when looking from 200 mm away).
    clip.z = clip.z - 0.00001 * clip.w;
    out.clip = clip;
    out.color = color;
    return out;
}

@fragment
fn fs_line(in: LineOut) -> @location(0) vec4<f32> {
    return in.color * u.color;
}
