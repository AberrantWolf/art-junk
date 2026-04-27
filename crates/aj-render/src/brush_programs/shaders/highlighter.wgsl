// Highlighter (multiplicative tint) deposit against the linear-RGB substrate.
//
// Standard "Multiply" blend with coverage modulation:
//   t        = coverage * brush_alpha
//   new.rgb  = canvas.rgb * mix(vec3(1.0), brush.rgb, t)
//            = canvas.rgb * (vec3(1.0) - t * (vec3(1.0) - brush.rgb))
//   new.a    = canvas.a + (1 - canvas.a) * t
//
// Behavior:
//   - Yellow highlighter over black (canvas.rgb = 0) stays black: 0 * y = 0.
//   - Yellow highlighter over white (canvas.rgb = 1) tints toward y * t.
//   - Overlapping strokes saturate further (real highlighters do this; we
//     don't cap because users coming from Procreate / Krita expect it).
//
// Uses the shared `Uniforms` struct so plain, pigment, and highlighter all
// bind the same group layout. `brush_lo` / `brush_hi` are unused here — they
// matter only to the pigment shader.

struct Uniforms {
    brush_lin: vec4<f32>,    // linear RGB + alpha (rgb used; alpha pulled from brush_alpha)
    brush_lo: vec4<f32>,     // unused by highlighter
    brush_hi: vec4<f32>,     // unused by highlighter
    brush_alpha: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var coverage_tex: texture_2d<f32>;
@group(0) @binding(2) var substrate: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    let xy = vec2<f32>(f32((vid << 1u) & 2u), f32(vid & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(xy.x, 1.0 - xy.y);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let coord = vec2<i32>(in.pos.xy);
    let coverage = textureLoad(coverage_tex, coord, 0).a;
    let t = clamp(coverage * uniforms.brush_alpha, 0.0, 1.0);

    let canvas = textureLoad(substrate, coord, 0);
    let tint = vec3<f32>(1.0) - t * (vec3<f32>(1.0) - uniforms.brush_lin.rgb);
    let new_rgb = canvas.rgb * tint;
    let new_alpha = canvas.a + (1.0 - canvas.a) * t;
    return vec4<f32>(new_rgb, new_alpha);
}
