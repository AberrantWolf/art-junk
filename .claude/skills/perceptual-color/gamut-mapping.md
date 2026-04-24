---
name: gamut-mapping
description: Strategies for bringing out-of-sRGB Oklab/Oklch colors into the display gamut without ugly hue shifts, with Ottosson's methods and the CSS Color 4 binary-search algorithm.
---

# Gamut mapping

Oklab and Oklch can address colors outside sRGB (and outside P3, and outside Rec2020). Any time the picker produces a color that has to be **rendered or stored as sRGB**, we have to map it into the sRGB gamut. The question is how.

## The failure mode to avoid

The naive answer — clamp each linear sRGB channel to `[0, 1]` — is cheap and **wrong**. Clipping `(1.1, -0.05, 0.3)` to `(1.0, 0.0, 0.3)` shifts hue noticeably. A slightly-too-saturated red becomes a markedly different, less-red red. A picker that does this will feel like the hue ring lies when the user cranks saturation.

## The family of correct answers

All the good strategies work in Oklab/Oklch, keeping **hue** constant, and trade off between two things:

- **Preserve lightness** (`L` fixed), reduce chroma until in gamut.
- **Preserve chroma** (`C` fixed), reduce lightness until in gamut.

In practice almost every real strategy leans on the first — saturation is more forgiving to reduce than brightness, because a slightly-less-saturated red still reads as "that red," while a noticeably-darker red reads as a different color.

### 1. Preserve-L, binary-search chroma (CSS Color 4 default)

The algorithm standardized by CSS Color Module Level 4:

1. If the color is already in gamut, return it.
2. Otherwise, binary-search on `C` with `L` and `h` fixed. At each step, clip the candidate to the sRGB cube as a safety net.
3. Compare the clipped candidate to the unclipped candidate via `deltaEOK` (Euclidean distance in Oklab).
4. If that distance is below the JND (just-noticeable difference, ~0.02), accept the clipped candidate. Otherwise tighten the chroma bounds.
5. Terminate after a fixed iteration budget (CSS says ~25 iterations).

The idea: reduce chroma just far enough that the sRGB-cube clip becomes indistinguishable from the Oklch-correct answer. This preserves lightness *and* avoids the hue shift that pure channel clipping would cause.

This is the strategy `palette` implements in `Clamp` / gamut-mapping helpers. It's also what you want by default in the picker's "commit" step.

### 2. Ottosson's strategies (from his gamut-clipping post)

Björn Ottosson catalogues several variants with closed-form cusp intersections:

- **Preserve L, project toward `(L, 0)`** — the "preserve lightness" flavor above, but done geometrically (intersect the `L = const` line with the gamut boundary). Cheap, no iteration. Desaturates very saturated colors to near-gray.
- **Project toward `(0.5, 0)`** — a single middle-gray anchor for every hue. Simple, less L-preserving.
- **Project toward `(L_cusp, 0)`** — toward the cusp (brightest in-gamut point) of the current hue. Tends to keep colors saturated at the expense of lightness.
- **Adaptive `L_0`** — interpolates between the above, parameterized by α ∈ [0, 1]. Ottosson recommends **α ≈ 0.05** as a practical default: behaves like preserve-L for moderate out-of-gamut colors, gracefully falls toward cusp projection for extreme cases.

These are closed-form (solve a cubic, use Halley's method to refine). Faster than binary search, slightly less precise. Good for real-time visualization of gamut in the picker where we render hundreds of pixels; binary search is fine for the one-shot commit.

## What the picker should do

Two separate jobs:

1. **Live gamut indication.** As the user drags sliders, show where the sRGB (and maybe P3) boundary falls. Ottosson's adaptive-α-0.05 is cheap enough for a GPU shader that fills the chroma/lightness plane.
2. **Commit to document.** When the user releases a color into a `BrushParams`, run the CSS Color 4 binary-search mapping once and store the result as in-gamut linear sRGB. The document should not store out-of-gamut colors — that would defer the clipping decision to every render.

## Cross-gamut futures

If we later support P3 or Rec2020 displays, the same algorithms work with the wider gamut as the "in-gamut" target. The only change is which gamut boundary the clip step checks against. Oklab coordinates are display-agnostic; the gamut is the per-display policy.

## Sources

- [sRGB gamut clipping — Björn Ottosson](https://bottosson.github.io/posts/gamutclipping/)
- [CSS Color Module Level 4 — §13 Gamut Mapping](https://www.w3.org/TR/css-color-4/#gamut-mapping)
- [Gamut mapping — Color.js docs](https://colorjs.io/docs/gamut-mapping)
