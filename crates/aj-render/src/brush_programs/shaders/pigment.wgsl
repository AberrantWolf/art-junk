// Kubelka–Munk pigment deposit against the linear-RGB substrate.
//
// The substrate stores straight-alpha linear RGB. To mix with K/S we
// upsample the substrate's RGB to a 7-band reflectance via the Smits
// basis, run K/S in band space, integrate back to linear RGB, and write.
//
// "Already dried" assumption: the substrate's current color is treated as
// the underneath state for this stroke's mix — there is no wet-on-wet
// diffusion. Re-upsampling RGB → bands at every deposit costs precision
// vs. holding spectral state, but the loss is bounded (~5e-4 round-trip
// drift on the in-gamut subset, per the parity test) and avoids carrying
// 7 channels around the pipeline.
//
// Brush color is upsampled on the host (`brush_lo`/`brush_hi` uniforms);
// `t = coverage * brush_alpha` is the per-pixel mix weight, identical to
// the plain shader. Alpha tracks "how much paint is on this pixel" so the
// present pass can composite the substrate over the backdrop.
//
// Constants (R/G/B basis, X/Y/Z matching curves, M_INV) mirror
// `aj_core::pigment::spectral` byte-for-byte. The host-side mirror in
// `parity.rs` enforces no drift via `host_upsample_matches_aj_core_for_primaries`.

struct Uniforms {
    brush_lin: vec4<f32>,    // unused by pigment
    brush_lo: vec4<f32>,     // bands 0..3 of brush reflectance
    brush_hi: vec4<f32>,     // bands 4..6 in xyz (w unused)
    brush_alpha: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var coverage_tex: texture_2d<f32>;
@group(0) @binding(2) var substrate: texture_2d<f32>;

const REFLECTANCE_FLOOR: f32 = 1e-3;
const REFLECTANCE_CEIL: f32 = 0.999999;

// Smits 7-band basis spectra — bands 0..3 in `_LO`, bands 4..6 in `_HI`.
const R_BASIS_LO: vec4<f32> = vec4<f32>(0.00, 0.00, 0.00, 0.05);
const R_BASIS_HI: vec3<f32> = vec3<f32>(0.35, 0.95, 1.00);
const G_BASIS_LO: vec4<f32> = vec4<f32>(0.00, 0.05, 0.65, 0.95);
const G_BASIS_HI: vec3<f32> = vec3<f32>(0.65, 0.05, 0.00);
const B_BASIS_LO: vec4<f32> = vec4<f32>(1.00, 0.95, 0.35, 0.00);
const B_BASIS_HI: vec3<f32> = vec3<f32>(0.00, 0.00, 0.00);

// CIE-like matching curves (band integration → XYZ).
const X_LO: vec4<f32> = vec4<f32>(0.00, 0.00, 0.05, 0.20);
const X_HI: vec3<f32> = vec3<f32>(0.40, 0.30, 0.05);
const Y_LO: vec4<f32> = vec4<f32>(0.00, 0.10, 0.35, 0.40);
const Y_HI: vec3<f32> = vec3<f32>(0.10, 0.05, 0.00);
const Z_LO: vec4<f32> = vec4<f32>(0.30, 0.40, 0.20, 0.05);
const Z_HI: vec3<f32> = vec3<f32>(0.05, 0.00, 0.00);

// XYZ → linear RGB post-correction.
const M_INV_R: vec3<f32> = vec3<f32>( 2.4574, -1.9719,  0.5145);
const M_INV_G: vec3<f32> = vec3<f32>(-0.3874,  1.9415, -0.5540);
const M_INV_B: vec3<f32> = vec3<f32>( 0.0533, -0.5428,  1.4895);

fn k_from_r(r_in: f32) -> f32 {
    let r = clamp(r_in, REFLECTANCE_FLOOR, REFLECTANCE_CEIL);
    let one_minus_r = 1.0 - r;
    return (one_minus_r * one_minus_r) / (2.0 * r);
}

fn r_from_k(k: f32) -> f32 {
    // S = 1 throughout v1, so K/S == K.
    let inner = max(k * k + 2.0 * k, 0.0);
    let r = 1.0 + k - sqrt(inner);
    return clamp(r, 0.0, 1.0);
}

fn mix_band(canvas_r: f32, brush_r: f32, t: f32) -> f32 {
    let canvas_k = k_from_r(canvas_r);
    let brush_k = k_from_r(brush_r);
    let mixed_k = mix(canvas_k, brush_k, t);
    return r_from_k(mixed_k);
}

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
    let r = max(canvas.r, 0.0);
    let g = max(canvas.g, 0.0);
    let b = max(canvas.b, 0.0);

    // Upsample canvas linear RGB to 7-band reflectance.
    let canvas_lo = vec4<f32>(
        r * R_BASIS_LO.x + g * G_BASIS_LO.x + b * B_BASIS_LO.x,
        r * R_BASIS_LO.y + g * G_BASIS_LO.y + b * B_BASIS_LO.y,
        r * R_BASIS_LO.z + g * G_BASIS_LO.z + b * B_BASIS_LO.z,
        r * R_BASIS_LO.w + g * G_BASIS_LO.w + b * B_BASIS_LO.w,
    );
    let canvas_hi = vec3<f32>(
        r * R_BASIS_HI.x + g * G_BASIS_HI.x + b * B_BASIS_HI.x,
        r * R_BASIS_HI.y + g * G_BASIS_HI.y + b * B_BASIS_HI.y,
        r * R_BASIS_HI.z + g * G_BASIS_HI.z + b * B_BASIS_HI.z,
    );

    // K/S mix in band space.
    let mixed_lo = vec4<f32>(
        mix_band(canvas_lo.x, uniforms.brush_lo.x, t),
        mix_band(canvas_lo.y, uniforms.brush_lo.y, t),
        mix_band(canvas_lo.z, uniforms.brush_lo.z, t),
        mix_band(canvas_lo.w, uniforms.brush_lo.w, t),
    );
    let mixed_hi = vec3<f32>(
        mix_band(canvas_hi.x, uniforms.brush_hi.x, t),
        mix_band(canvas_hi.y, uniforms.brush_hi.y, t),
        mix_band(canvas_hi.z, uniforms.brush_hi.z, t),
    );

    // Integrate bands → XYZ, post-correct → linear RGB.
    let x_val = dot(mixed_lo, X_LO) + dot(mixed_hi, X_HI);
    let y_val = dot(mixed_lo, Y_LO) + dot(mixed_hi, Y_HI);
    let z_val = dot(mixed_lo, Z_LO) + dot(mixed_hi, Z_HI);
    let xyz = vec3<f32>(x_val, y_val, z_val);

    let new_lin = vec3<f32>(
        clamp(dot(M_INV_R, xyz), 0.0, 1.0),
        clamp(dot(M_INV_G, xyz), 0.0, 1.0),
        clamp(dot(M_INV_B, xyz), 0.0, 1.0),
    );

    let new_alpha = canvas.a + (1.0 - canvas.a) * t;
    return vec4<f32>(new_lin, new_alpha);
}
