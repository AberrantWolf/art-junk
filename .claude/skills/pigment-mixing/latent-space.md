---
name: latent-pigment-mixing
description: Conceptual summary of Sochorová & Jamriška's 2021 latent-space pigment mixing (the Mixbox algorithm), the licensing situation, and what a clean-room reimplementation looks like for an MIT/Apache codebase.
---

# Latent-space pigment mixing

> **Licensing**: The reference implementation, **[Mixbox](https://github.com/scrtwpns/mixbox)**, is **CC BY-NC 4.0** — non-commercial only, incompatible with art-junk's `MIT OR Apache-2.0` license. Do not vendor Mixbox source or its LUT PNG. This file describes the published algorithm conceptually. See `SKILL.md` for the full licensing analysis.

## The problem the paper solves

Plain Kubelka-Munk (see `kubelka-munk.md`) gives correct pigment behavior but has two practical problems for a consumer drawing app:

1. **Runtime cost.** Even 7-bin spectral mixing is an order of magnitude slower than an additive blend.
2. **Color fidelity at the endpoints.** If a user picks RGB `(0.2, 0.05, 0.3)` — a very specific deep purple — and K-M mixes that with white, the result should start at *exactly* that purple. But "exactly that purple" may not lie on any physically plausible reflectance spectrum, so the spectral-upsampling step already shifts the endpoint slightly. After K-M mixing and round-trip back, the result drifts.

Sochorová & Jamriška's 2021 paper, *Practical Pigment Mixing for Digital Painting* (ACM TOG 40(6), SIGGRAPH Asia 2021), frames a **latent-space decomposition** that fixes both.

## The idea in one paragraph

Every RGB color is re-expressed as a pair: **(concentrations of a small set of primary pigments, additive residual)**. The concentrations describe the pigment-like part of the color; the residual is an RGB offset that captures whatever couldn't be explained by the pigments. Mixing two colors is **lerp of concentrations** (K-M behavior emerges naturally, since K-M is linear in concentration) **+ lerp of residuals** (additive, preserves endpoint fidelity). Reconstructing an RGB from a latent pair: render the concentrations via K-M to get a pigment-based RGB, then add the residual.

Because the decomposition is expensive (per color, an optimization problem) and the set of primary pigments is fixed, the authors **precompute the decomposition for every RGB** into a 3D LUT. At runtime, mixing two colors is two LUT fetches, two lerps, and one K-M reconstruction — cheap enough for a fragment shader.

## Stepwise structure

Per the paper (conceptual; paraphrased, no code copied):

1. **Choose a small primary pigment palette** (Mixbox ships 16 pigments; fewer work, more give the LUT more expressiveness).
2. **Precompute**: for each RGB `(r, g, b)` on a regular 3D grid, solve for concentrations `c_i ≥ 0` (summing to 1 — or similar constraint) of the primary pigments and an additive residual `(Δr, Δg, Δb)` such that:
   - Rendering the concentrations through K-M yields some intermediate RGB `I`.
   - `I + (Δr, Δg, Δb) = (r, g, b)` exactly.
   - The residual is minimized in some norm (so concentrations absorb as much of the color as they physically can).
3. **Store** the per-voxel `(c_1, ..., c_N, Δr, Δg, Δb)` as a 3D LUT (or a compact encoding thereof).
4. **At runtime**, to mix `color_a` and `color_b` at parameter `t`:
   - Fetch `(c_a, Δ_a)` and `(c_b, Δ_b)` from the LUT.
   - `c_mix = lerp(c_a, c_b, t)`, `Δ_mix = lerp(Δ_a, Δ_b, t)`.
   - Reconstruct: run `c_mix` through K-M to an intermediate RGB, add `Δ_mix`, clamp/gamut-map, return.

## What this buys

- **RGB fidelity.** Endpoints reconstruct *exactly* (because the residual is chosen to close any round-trip error).
- **K-M behavior in the middle.** Yellow + blue = green, violet + yellow = muddy red-brown, white + ultramarine = chalky blue.
- **Runtime cheapness.** Two texture samples, a lerp, a short K-M inversion. Close to additive-mixing cost, way under spectral K-M.

## Clean-room reimplementation path

If we decide tier 3 is worth the engineering investment for art-junk, the path is:

1. **Patent search.** Confirm no active patent covers the latent-decomposition technique. (Not legal advice — involve an actual lawyer if the project turns commercial.)
2. **Pick primaries.** Our own palette. Published K/S tables for, say, Titanium White, Cadmium Yellow, Pyrrol Red, Ultramarine Blue, Phthalo Green, Carbon Black. 6–10 pigments is enough.
3. **Solve the decomposition offline.** For each voxel on a regular RGB grid (e.g., 257³, or a sparser grid with trilinear interpolation), solve the constrained optimization described in the paper: non-negative concentrations, small residual. An off-the-shelf LP/NNLS solver in `ndarray` / `nalgebra` handles this. Hours-scale precompute.
4. **Ship our own LUT** as `aj-engine` data, Apache-2.0-licensed because we generated it. Cite the paper in the crate's README as the methodology source.
5. **Runtime: WGSL pass in `aj-effects`** that fetches from the LUT texture and reconstructs. CPU fallback on the engine worker path for the "resolve" step of a stroke.

Rough effort estimate, solo: 2–4 weeks to a working prototype, another 2–4 weeks to tune the primary palette and measurement choices so that mixing results match published paint charts well enough to feel right.

## Alternatives if we don't want to do the clean-room work

- **Ship tier 2 only.** Plain Kubelka-Munk with a spectral upsampling (Meng or Jakob-Hanika) and a modest primary database. Less polished than tier 3 but correct paint behavior, fully license-clean, and a lot less code.
- **Keep the `mixbox-preview` feature flag for personal / development builds only.** Users who fork for personal use benefit; the distributed build has tier 2.
- **Wait for an open-licensed reimplementation.** None has appeared as of this skill's writing; not a reliable plan.

## Sources

- Sochorová & Jamriška (2021). Practical Pigment Mixing for Digital Painting. *ACM TOG* 40(6), Article 234 (SIGGRAPH Asia 2021). [Project page](https://dcgi.fel.cvut.cz/en/publications/2021/sochorova-tog-pigments/) · DOI `10.1145/3478513.3480549`.
- [Mixbox — GitHub](https://github.com/scrtwpns/mixbox) (reference; **CC BY-NC 4.0 — do not vendor**).
- [Mixbox talk — SIGGRAPH Asia 2021 (YouTube)](https://www.youtube.com/watch?v=_qa5iWdfNKg)
