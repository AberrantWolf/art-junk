// Composite the substrate and chrome onto the surface.
//
// Substrate stores straight-alpha linear RGB. We composite it over the
// backdrop in linear space (the right place to do alpha blending), then
// sRGB-encode for the *_Unorm surface. Chrome (page border, future UI
// decorations) is rendered by Vello to an `Rgba8Unorm` texture; per Vello
// convention, those bytes are already sRGB-encoded with straight alpha,
// so we composite chrome on top in sRGB space after encoding the
// substrate stage.

@group(0) @binding(0) var substrate: texture_2d<f32>;
@group(0) @binding(1) var chrome: texture_2d<f32>;

// Dark surface backdrop the canvas sits on top of. Stored as the sRGB
// byte triple; converted to linear at composite time so we can blend in
// the right space.
const BACKDROP_SRGB: vec3<f32> = vec3<f32>(15.0 / 255.0, 18.0 / 255.0, 23.0 / 255.0);

fn srgb_to_linear_channel(c: f32) -> f32 {
    if (c <= 0.04045) {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

fn linear_to_srgb_channel(c: f32) -> f32 {
    let x = clamp(c, 0.0, 1.0);
    if (x <= 0.0031308) {
        return 12.92 * x;
    }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
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

    // Substrate-over-backdrop in linear, then sRGB-encode.
    let s = textureLoad(substrate, coord, 0);
    let backdrop_lin = vec3<f32>(
        srgb_to_linear_channel(BACKDROP_SRGB.x),
        srgb_to_linear_channel(BACKDROP_SRGB.y),
        srgb_to_linear_channel(BACKDROP_SRGB.z),
    );
    let composed_lin = mix(backdrop_lin, s.rgb, clamp(s.a, 0.0, 1.0));
    let composed_srgb = vec3<f32>(
        linear_to_srgb_channel(composed_lin.x),
        linear_to_srgb_channel(composed_lin.y),
        linear_to_srgb_channel(composed_lin.z),
    );

    // Chrome-over-(substrate+backdrop) in sRGB. Vello's fine shader
    // unpremultiplies before textureStore (vello_shaders/shader/fine.wgsl),
    // so chrome.rgb is straight sRGB and `mix` is the correct alpha-over.
    let c = textureLoad(chrome, coord, 0);
    let final_srgb = mix(composed_srgb, c.rgb, c.a);

    return vec4<f32>(final_srgb, 1.0);
}
