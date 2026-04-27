// Inter-layer Normal blend: standard alpha-over with per-layer opacity.
//
//   t        = layer.alpha * uniforms.opacity
//   new.rgb  = mix(below.rgb, layer.rgb, t)
//   new.a    = below.a + (1 - below.a) * t
//
// Both inputs are straight-alpha linear RGB; output is the same. Per-layer
// opacity multiplies into `t` rather than scaling `layer.rgb` because the
// substrate's RGB is the paint color (only meaningful where alpha > 0) —
// scaling it would dim translucent paint instead of letting more of below
// show through, which is the wrong semantic for "Normal" blend.
//
// `Unknown` blend modes fall back to this pipeline at the host (see
// `composite_layers` in mod.rs), so this is also the safe default.

struct Uniforms {
    opacity: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var below: texture_2d<f32>;
@group(0) @binding(2) var layer: texture_2d<f32>;

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
    let b = textureLoad(below, coord, 0);
    let l = textureLoad(layer, coord, 0);

    let t = clamp(l.a * uniforms.opacity, 0.0, 1.0);
    let new_rgb = mix(b.rgb, l.rgb, t);
    let new_alpha = b.a + (1.0 - b.a) * t;
    return vec4<f32>(new_rgb, new_alpha);
}
