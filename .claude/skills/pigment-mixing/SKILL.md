---
name: pigment-mixing
description: Reference for pigment-style color mixing in art-junk — why paints mix differently from light, the Kubelka-Munk foundation, the Sochorová/Jamriška 2021 latent-space approach, and the licensing constraints that govern what we can ship.
---

# Pigment mixing — reference

## Why this matters for a drawing app

Additive mixing (the math RGB does natively) is how **light** mixes: blue light plus yellow light makes whitish light. **Paint** does not mix that way: blue pigment plus yellow pigment makes green, because pigments subtract wavelengths from reflected light rather than adding them. A drawing app whose colors claim to behave like paint but mix like projector lamps breaks the user's intuition at exactly the moments a painter reaches for it — wet-on-wet blending, glazing, color wheels built from complementary primaries.

art-junk intends to offer pigment-style mixing as a first-class option alongside additive mixing. This skill is the conceptual reference.

## Three tiers of sophistication

| Tier | What it does | Cost | "Yellow + Blue =" |
|---|---|---|---|
| **1. Oklab interpolation** | Perceptual midpoint in Oklab | 1 matrix op per sample | Greenish-gray (perceptually smoother than sRGB lerp, still additive) |
| **2. Spectral Kubelka-Munk** | Real K-M physics over a sampled spectrum, with a pigment database | ~10–100× tier 1 | Green (correct paint behavior) |
| **3. Latent-space pigment mixing** | Sochorová/Jamriška 2021: RGB → (pigment concentrations, residual) → linear ops → RGB | ~5–10× tier 1, LUT-driven | Green (K-M-like result with RGB-speed I/O) |

Tier 1 is always on — our color picker lives in Oklch, so any "interpolate two colors" operation gets tier 1 for free. Tier 2 is a clean, fully-public-domain option we can ship. Tier 3 is the state of the art — but see the **licensing** section below before planning to use it.

- [kubelka-munk.md](kubelka-munk.md) — the physics; how K/S space works; how to go RGB → spectrum → K/S → spectrum → RGB.
- [latent-space.md](latent-space.md) — the Sochorová/Jamriška approach at a conceptual level; what a clean-room reimplementation looks like; why we don't vendor Mixbox.

---

## Licensing — read this first

The reference implementation of tier 3 is **[Mixbox](https://github.com/scrtwpns/mixbox)** by Secret Weapons (Sochorová and Jamriška's company). Mixbox ships:

- Source code (C / GLSL / JS / Rust bindings) — **licensed CC BY-NC 4.0**.
- `mixbox_lut.png` — a 3D lookup table encoding the pigment decomposition — **licensed CC BY-NC 4.0**.

CC BY-NC 4.0 is **non-commercial only** and **not compatible** with art-junk's `MIT OR Apache-2.0` workspace license. We cannot:

- Add `mixbox` as a Cargo dependency and ship it.
- Vendor the source into a crate.
- Redistribute the LUT as a binary asset.

We *can*:

- Use Mixbox locally, for personal experimentation, off the main branch.
- Cite the paper and discuss the algorithm in design notes.
- Implement tier 2 (plain Kubelka-Munk) from first principles — K-M physics is 1930s and public-domain.
- Clean-room reimplement tier 3 based only on the published paper, with our own primary pigments and our own LUT, citing the paper for methodology. See [latent-space.md](latent-space.md).

> Before committing to the clean-room path, do a patent search. A casual search shows nothing filed by Secret Weapons, and the paper and commercial-license wording imply their moat is copyright of the LUT and code rather than a patent, but that is **not legal advice** and needs checking before the code leaves the project.

If someone on the team wants Mixbox's behavior quickly for a demo, the right move is a feature-flag-gated crate (`feature = "mixbox-preview"`) that pulls Mixbox as a dev-only, non-commercial dependency — not a default-on part of the distribution.

## Where in the architecture

Tier 1 (Oklab) is a property of the **color picker** and any gradient UI — the `perceptual-color` skill covers it.

Tiers 2 and 3 are properties of a **brush mixing mode**. Two brushes should be able to produce different mixing behavior when they overlap a canvas region. This maps cleanly onto the existing `BrushParams` structure — add a `mixing_mode: MixingMode` field with variants like `Additive`, `Pigment` (and room for `Pigment { kind: ... }` if we support multiple pigment algorithms). The stroke rasterizer queries the mode and dispatches to the appropriate blend.

Because pigment mixing is more expensive than additive mixing, and because it's doing per-pixel work, it belongs on the GPU via a WGSL pass in `aj-effects`, *or* on the CPU via a worker in `aj-engine` for the "resolve full brush texturing off-thread" path described in `CLAUDE.md`. The placeholder / resolve split in the latency strategy is exactly the right seam: paint a cheap additive placeholder in <1ms, resolve pigment mixing in a worker, swap in.

## UX considerations

- **Mixing mode is not a global setting** — it's per-brush. A highlighter should stay additive (overlapping yellow onto blue stays yellow-on-blue-ish, not green). An "oil paint" brush should use pigment mixing.
- **Make the default mode honest.** The first brush the user meets should mix the way they expect. For a "paint-first" app the default is pigment; for an "illustration-first" app the default is additive. art-junk's framing (stylus-first, drawing program, paint vocabulary) argues for pigment as default.
- **Don't bury the switch.** A brush inspector's mixing-mode dropdown needs to be discoverable, not hidden in a palette's settings submenu. Users will want to A/B the two modes.
- **Show the primary pigments somewhere.** Tier 2 and tier 3 both work off a finite palette of primaries. Exposing "this brush's pigment decomposition" as a small swatch row in the inspector teaches the user what's happening and gives us somewhere to explain why "crimson + cerulean = violet, not brown."

## Further reading

- Sochorová & Jamriška (2021). Practical Pigment Mixing for Digital Painting. *ACM TOG* 40(6), Article 234 (SIGGRAPH Asia 2021). [Project page](https://dcgi.fel.cvut.cz/en/publications/2021/sochorova-tog-pigments/) · DOI `10.1145/3478513.3480549`.
- [Kubelka–Munk theory — Wikipedia](https://en.wikipedia.org/wiki/Kubelka%E2%80%93Munk_theory)
- [Mixbox — GitHub](https://github.com/scrtwpns/mixbox) (reference, **CC BY-NC 4.0 — do not vendor**).
- Meng, Simon, Hanika, Dachsbacher (2015). Physically Meaningful Rendering using Tristimulus Colours. *EGSR 2015.* — spectral upsampling technique we would use for tier 2.
