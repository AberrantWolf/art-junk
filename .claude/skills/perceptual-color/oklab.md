---
name: oklab-math
description: The forward and inverse Oklab transforms with the exact matrices, the cylindrical Oklch form, and Rust implementation notes.
---

# Oklab math

## Forward: linear sRGB → Oklab

Input `(R, G, B)` is **linear** (not gamma-encoded) and in `[0, 1]` for in-gamut sRGB colors. It may also be negative or > 1 for out-of-sRGB colors.

**Step 1.** To a cone-like LMS space via `M1`:

```
┌ l ┐     ┌ 0.4122214708  0.5363325363  0.0514459929 ┐ ┌ R ┐
│ m │  =  │ 0.2119034982  0.6806995451  0.1073969566 │ │ G │
└ s ┘     └ 0.0883024619  0.2817188376  0.6299787005 ┘ └ B ┘
```

**Step 2.** Component-wise cube root:

```
l' = cbrt(l)
m' = cbrt(m)
s' = cbrt(s)
```

Use `f32::cbrt` / `f64::cbrt`, which handles the negative case correctly (`cbrt(-x) = -cbrt(x)`). Do **not** use `powf(1.0 / 3.0)` — it returns NaN for negatives.

**Step 3.** To Oklab via `M2`:

```
┌ L ┐     ┌  0.2104542553   0.7936177850  -0.0040720468 ┐ ┌ l' ┐
│ a │  =  │  1.9779984951  -2.4285922050   0.4505937099 │ │ m' │
└ b ┘     └  0.0259040371   0.7827717662  -0.8086757660 ┘ └ s' ┘
```

`L` ends up in `[0, 1]` for in-gamut colors. `a` and `b` range roughly `[-0.4, +0.4]`; CSS Color 4 treats `±100% = ±0.4`.

## Inverse: Oklab → linear sRGB

Apply `M2⁻¹`, cube each component, apply `M1⁻¹`. The inverses are reported by Ottosson; `palette` has them baked in. A naive `glam::Mat3::inverse` also works because neither matrix is ill-conditioned.

## Cylindrical form: Oklch

```
C = sqrt(a² + b²)
h = atan2(b, a)   // radians, or convert to degrees
```

Going back:

```
a = C * cos(h)
b = C * sin(h)
```

Near `C = 0`, hue is undefined; preserve the previous hue in the picker so the slider doesn't jump when the user passes through neutral.

## Rust sketch (hand-rolled)

Only needed if we don't want the `palette` dep. Keep the matrices as `const` in a small module.

```rust
const M1: [[f32; 3]; 3] = [
    [0.4122214708, 0.5363325363, 0.0514459929],
    [0.2119034982, 0.6806995451, 0.1073969566],
    [0.0883024619, 0.2817188376, 0.6299787005],
];

const M2: [[f32; 3]; 3] = [
    [0.2104542553,  0.7936177850, -0.0040720468],
    [1.9779984951, -2.4285922050,  0.4505937099],
    [0.0259040371,  0.7827717662, -0.8086757660],
];

pub fn linear_srgb_to_oklab([r, g, b]: [f32; 3]) -> [f32; 3] {
    let l = M1[0][0]*r + M1[0][1]*g + M1[0][2]*b;
    let m = M1[1][0]*r + M1[1][1]*g + M1[1][2]*b;
    let s = M1[2][0]*r + M1[2][1]*g + M1[2][2]*b;

    let (l, m, s) = (l.cbrt(), m.cbrt(), s.cbrt());

    [
        M2[0][0]*l + M2[0][1]*m + M2[0][2]*s,
        M2[1][0]*l + M2[1][1]*m + M2[1][2]*s,
        M2[2][0]*l + M2[2][1]*m + M2[2][2]*s,
    ]
}
```

Inverse is symmetric with the inverse matrices and a cube (`x * x * x`) in place of the cube root.

## Common pitfalls

- **Non-linear input.** Gamma-encoded sRGB (what a PNG stores, what `peniko::Color::rgba8` hands you) must be linearized first. `palette` does this automatically if you type the input as `Srgb<f32>`; hand-rolled code must call the piecewise linearization.
- **Alpha.** Oklab has nothing to say about alpha. Carry it alongside the `Oklab` struct; premultiply only in linear sRGB.
- **Cube root of negatives.** `cbrt` handles it. `powf(1/3)` does not.
- **Hue in radians vs degrees.** `atan2` returns radians in `(-π, π]`. CSS and design tools use degrees in `[0, 360)`. Normalize once at the API boundary.
- **Chroma is unbounded.** `C` is not normalized — it depends on how saturated a color is in the target gamut. Don't put a 0–1 slider on it; pick a per-hue max or clamp by gamut (see gamut-mapping).

## Test vectors

From Ottosson's reference:

| Input (linear sRGB) | Oklab |
|---|---|
| `(1.0, 1.0, 1.0)` | `(1.000, 0.000, 0.000)` |
| `(1.0, 0.0, 0.0)` | `(0.628,  0.225,  0.126)` |
| `(0.0, 1.0, 0.0)` | `(0.866, -0.234,  0.179)` |
| `(0.0, 0.0, 1.0)` | `(0.452, -0.032, -0.312)` |

Put these in a unit test in the color utility module.
