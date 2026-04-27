// Inter-layer Oklab Mix blend: Porter-Duff over with the RGB lerp moved
// into Oklab space (Björn Ottosson 2020) for perceptually-uniform midpoints.
//
//   t          = layer.alpha * uniforms.opacity
//   below_lab  = linear_rgb_to_oklab(below.rgb)
//   layer_lab  = linear_rgb_to_oklab(layer.rgb)
//   mixed_lab  = mix(below_lab, layer_lab, t)
//   new.rgb    = clamp(oklab_to_linear_rgb(mixed_lab), 0, 1)
//   new.a      = below.a + (1 - below.a) * t      // same as Normal
//
// Why Oklab not OKLCH? OKLCH is polar (L, chroma, hue). Lerping with hue
// can take the long way around the colour wheel; the user gets unexpected
// midpoint hues. Oklab is rectangular — straight lerp gives the visually-
// expected midpoint. (Yellow + blue under Oklab → a neutral mid-gray that
// looks correct, not the saturated greenish-purple OKLCH would produce.)
//
// Known limitation — gamut clipping. An Oklab midpoint between two
// in-gamut linear-RGB endpoints can fall *outside* the linear-RGB cube
// (e.g. green + blue at t=0.5 produces a slightly-negative red channel).
// The final `clamp` to [0, 1] hides this by desaturating the result
// rather than producing a true Oklab mid-colour. A proper gamut map
// (Ottosson's chroma-reduction post) is the right fix; not in scope
// for the C4 ship.
//
// Constants are Ottosson's published matrices, byte-for-byte. The cbrt
// step lacks a built-in in WGSL; we use sign-preserving `sign(x) *
// pow(abs(x), 1/3)` so substrate K/S overshoot (slightly-negative RGB)
// doesn't NaN-trap the shader.

struct Uniforms {
    opacity: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var below: texture_2d<f32>;
@group(0) @binding(2) var layer: texture_2d<f32>;

fn signed_cbrt(x: f32) -> f32 {
    return sign(x) * pow(abs(x), 1.0 / 3.0);
}

// Linear RGB → Oklab.
fn linear_rgb_to_oklab(rgb: vec3<f32>) -> vec3<f32> {
    // RGB → LMS (Ottosson 2020).
    let l = 0.4122214708 * rgb.x + 0.5363325363 * rgb.y + 0.0514459929 * rgb.z;
    let m = 0.2119034982 * rgb.x + 0.6806995451 * rgb.y + 0.1073969566 * rgb.z;
    let s = 0.0883024619 * rgb.x + 0.2817188376 * rgb.y + 0.6299787005 * rgb.z;

    let l_ = signed_cbrt(l);
    let m_ = signed_cbrt(m);
    let s_ = signed_cbrt(s);

    // LMS^(1/3) → Lab.
    let lab_l =  0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_;
    let lab_a =  1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_;
    let lab_b =  0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_;
    return vec3<f32>(lab_l, lab_a, lab_b);
}

// Oklab → linear RGB.
fn oklab_to_linear_rgb(lab: vec3<f32>) -> vec3<f32> {
    // Lab → LMS^(1/3).
    let l_ = lab.x + 0.3963377774 * lab.y + 0.2158037573 * lab.z;
    let m_ = lab.x - 0.1055613458 * lab.y - 0.0638541728 * lab.z;
    let s_ = lab.x - 0.0894841775 * lab.y - 1.2914855480 * lab.z;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    // LMS → RGB.
    let r =  4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s;
    let g = -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s;
    let b = -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s;
    return vec3<f32>(r, g, b);
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
    let b = textureLoad(below, coord, 0);
    let l = textureLoad(layer, coord, 0);

    let t = clamp(l.a * uniforms.opacity, 0.0, 1.0);

    let below_lab = linear_rgb_to_oklab(b.rgb);
    let layer_lab = linear_rgb_to_oklab(l.rgb);
    let mixed_lab = mix(below_lab, layer_lab, t);
    let new_rgb = clamp(oklab_to_linear_rgb(mixed_lab), vec3<f32>(0.0), vec3<f32>(1.0));

    let new_alpha = b.a + (1.0 - b.a) * t;
    return vec4<f32>(new_rgb, new_alpha);
}
