---
name: perceptual-color
description: Reference for perceptually-uniform color work in art-junk — Oklab / Oklch, why they beat HSL for a color picker, gamut mapping from extended gamuts down to sRGB/P3, and where these fit relative to the app's linear-float working space.
---

# Perceptual color — Oklab / Oklch reference

art-junk stores and renders colors in a **linear-float working space** (sRGB primaries). That's the right space for blending pixels, scaling alpha, and feeding Vello. It is the *wrong* space for any UI that asks a human to reason about color:

- Sliding lightness in HSL shifts hue.
- Interpolating two hues in sRGB goes through muddy gray.
- Two colors with the same HSL "saturation" look wildly different in saturation to the eye.

**Oklab** (Björn Ottosson, 2020) and its cylindrical form **Oklch** are the current best answer. They are:

- **Perceptually uniform** (to a good first order) — Euclidean distance in Oklab approximates perceived color difference.
- **Cheap** — a 3×3 matrix, a cube root, a second 3×3 matrix, done. No LUT, no iterative solve.
- **Standardized** — CSS Color Module Level 4 defines `oklab()` and `oklch()`; browsers implement them; design tools (Figma, the `oklch.com` picker) expose them to users.

Use this skill when designing the color picker, building gradients, generating palettes, computing "nearest color" or delta-E, sorting swatches, or any other color operation that should match human perception. Do **not** use these for per-pixel compositing — blending happens in linear sRGB, always.

- [oklab.md](oklab.md) — the math: matrices, cube root, inverse, implementation notes for Rust.
- [gamut-mapping.md](gamut-mapping.md) — mapping Oklab / Oklch → sRGB (or P3) without ugly hue shifts, via Ottosson's strategies and the CSS Color 4 binary-search algorithm.

---

## Where in the pipeline

```
[ UI / picker / palette ]  ←  Oklch  (what a designer manipulates)
          │
          ▼  convert on commit
[ Document / BrushParams ]  ←  linear sRGB float  (what the renderer blends)
          │
          ▼  Vello / wgpu
[ Framebuffer ]             ←  sRGB encoded   (what the display shows)
```

The conversion point is *entering or leaving the document*. Inside the engine, everything is linear float (see `BrushParams` post-#d4d2689). Inside the picker, everything is Oklch. The boundary is explicit and one-way per direction.

## Why Oklch, not Oklab, for UI

Oklab is `(L, a, b)` — rectangular. Oklch is `(L, C, h)` — cylindrical. For user-facing controls:

- **L slider**: lightness, same perceived value across the whole picker.
- **C slider**: chroma, "colorfulness" — zero is neutral gray at the current L.
- **H ring**: hue, perceptually uniform; angular distance ≈ perceived hue distance.

HSL fails all three. HSV is worse. Oklch gives a color picker whose three axes are actually independent to the eye.

## Gamut awareness

Oklch can describe colors outside sRGB (and outside P3, and outside Rec2020). A picker that lets the user move freely in Oklch **must** show gamut boundaries and gamut-map the color when committing it to a document. See [gamut-mapping.md](gamut-mapping.md). For art-junk's v1 target (sRGB display, linear-float working), every picked color must be brought into sRGB before it becomes a `BrushParams::color`.

## Choosing a crate

- **`palette`** (MIT OR Apache-2.0) — full-featured color crate with first-class `Oklab` and `Oklch` types, conversions, and gamut mapping helpers. Dual-licensed, license-compatible with art-junk. Default choice.
- **`peniko::Color`** — sRGB-based; the Vello/Linebender stack uses it. Convert at the boundary, don't try to extend `peniko::Color` with Oklab operations.
- **Hand-rolled** — 30 lines of Rust is enough for forward + inverse Oklab. Reasonable if we want zero dep and full control of gamut behavior; see [oklab.md](oklab.md) for the matrices.

Default: `palette` for the picker, hand-rolled helper only if we find `palette`'s gamut mapping doesn't do what we want.

## Relation to delta-E and nearest-color queries

Euclidean distance in Oklab ≈ delta-E-OK, the perceptual difference metric CSS Color 4 uses for gamut mapping. For "find the nearest swatch in the palette," an L2 distance in Oklab is the correct, cheap metric. Do **not** use L2 in sRGB or HSL — both give the wrong answer for perceptual nearness.

## Non-goals here

- **HDR / wide-gamut rendering.** v1 is sRGB display; this skill assumes that. Oklab extends cleanly to P3 and Rec2020 later without redesign.
- **ICC profiles.** Still a v1 non-goal per `CLAUDE.md`.
- **CAM16 / JzAzBz / other perceptual spaces.** Oklab is empirically competitive with more complex models, much cheaper, and standardized. Don't swap unless we hit a specific failure case.

## Sources

- [Oklab color space — Wikipedia](https://en.wikipedia.org/wiki/Oklab_color_space)
- [A perceptual color space for image processing — Björn Ottosson](https://bottosson.github.io/posts/oklab/)
- [sRGB gamut clipping — Björn Ottosson](https://bottosson.github.io/posts/gamutclipping/)
- [CSS Color Module Level 4 — W3C](https://www.w3.org/TR/css-color-4/)
- [oklch.com picker](https://oklch.com/)
