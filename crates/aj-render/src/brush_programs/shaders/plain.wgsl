// Alpha-over deposit against the linear-RGB substrate.
//
// Reads the current substrate and Vello-rasterized coverage, mixes the
// brush color in straight-alpha linear RGB, writes the new substrate.
//   t        = coverage * brush_alpha
//   new.rgb  = mix(substrate.rgb, brush.rgb, t)
//   new.a    = substrate.a + (1 - substrate.a) * t
//
// Substrate alpha tracks "how much paint has been deposited at this pixel";
// the present pass uses it to composite the substrate over the backdrop.
//
// `brush_lo` / `brush_hi` are unused by this shader. They exist on the
// uniform struct so plain and pigment programs can share one bind-group
// layout and one uniform buffer — keeps the host code free of per-program
// branching at submission time.

struct Uniforms {
    brush_lin: vec4<f32>,    // linear RGB + alpha (rgb used; alpha pulled from brush_alpha for clarity)
    brush_lo: vec4<f32>,     // unused by plain
    brush_hi: vec4<f32>,     // unused by plain
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
    let new_rgb = mix(canvas.rgb, uniforms.brush_lin.rgb, t);
    let new_alpha = canvas.a + (1.0 - canvas.a) * t;
    return vec4<f32>(new_rgb, new_alpha);
}
