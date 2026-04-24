---
name: kubelka-munk
description: The Kubelka-Munk two-flux model for pigment reflectance, how to mix pigments in K/S space, and the RGB ↔ spectrum conversion needed to use it in a digital painting tool.
---

# Kubelka-Munk pigment mixing

Kubelka-Munk (K-M) is a two-flux model of light passing through a pigment layer, published in 1931 by Paul Kubelka and Franz Munk. It is **public domain** physics, used across the coatings industry for color matching, and perfectly fine to reimplement in an MIT/Apache codebase.

## The core relations

For a thick (opaque) pigment layer at a single wavelength:

- `K(λ)` — absorption coefficient, how strongly the layer absorbs light at wavelength λ.
- `S(λ)` — scattering coefficient, how strongly it scatters light back.
- `R∞(λ)` — reflectance at infinite thickness.

The K-M relations:

```
R∞(λ) = 1 + K/S − sqrt( (K/S)² + 2·(K/S) )

K/S = (1 − R)² / (2·R)
```

`K/S` is the natural "mixing space" — it is **linear in pigment concentration**. Mix *n* pigments with concentrations `c_i`:

```
(K/S)_mix(λ) = Σ c_i · (K_i(λ) / S_i(λ))
```

Then invert the K-M relation to get `R∞,mix(λ)`, the reflectance spectrum of the mixture. Convert that spectrum to XYZ → linear sRGB → display.

That's the whole algorithm. The hard parts are the data and the conversions.

## Sampling the spectrum

K, S, and R are functions of wavelength. In practice you discretize:

- **31 bins** (400–700 nm, every 10 nm): the standard CIE sampling. Accurate, heavy.
- **10 bins**: often good enough for painting-quality mixing.
- **7 bins** (Meng et al., or custom placements): sweet spot — around 7 carefully chosen wavelengths reproduce sRGB to within a small visible error for plausible reflectance spectra.

Per-pixel cost scales linearly with bin count. 7 bins is the right target for real-time; 31 for offline / golden comparison.

## The RGB ↔ spectrum boundary problem

Our document stores **RGB**. Pigment mixing has to happen in **spectral** space. So we need both directions:

- **Spectrum → RGB**: well-defined. Integrate `R(λ) · I(λ) · x̄(λ) dλ` for each CIE color-matching function (`x̄`, `ȳ`, `z̄`) under an illuminant `I` (typically D65), giving XYZ; then XYZ → linear sRGB by a known matrix.
- **RGB → spectrum**: ill-posed. Infinitely many spectra map to the same RGB (metamers). We pick one — ideally one that, when passed through K-M mixing and back, gives a result close to what a painter would expect. Standard techniques:
  - **Smits 1999** — the classic: a handful of smooth basis spectra (white, red, green, blue, cyan, magenta, yellow) linearly combined.
  - **Meng, Simon, Hanika, Dachsbacher 2015** — "Physically Meaningful Rendering using Tristimulus Colours," EGSR 2015. Produces smooth, physically plausible spectra via an optimization. Widely used.
  - **Jakob & Hanika 2019** — "A Low-Dimensional Function Space for Efficient Spectral Upsampling," Eurographics 2019. Fits a 3-coefficient polynomial spectrum per RGB; very cheap at runtime after a one-time fit.

For v1, Smits or Meng is more than adequate. Jakob-Hanika is the right choice if we decide we want spectral rendering across the board (we probably don't in v1).

## Building a pigment database

The "KS table" is the payload: for each primary pigment you support, a vector of `K(λ)` and `S(λ)` samples at your chosen bins. Where to get them:

- **Published data.** Colourlex, Pigments Through the Ages, vendor tech sheets, conservation literature, and older graphics-research papers (Curtis et al. 1997, Baxter et al. 2004) contain measured K and S for common pigments. Verify license: CIE data is fine; individual vendor measurements may carry restrictions.
- **Measure.** A spectrophotometer plus a white tile and an opaque draw-down gives `R∞` for each pigment. Invert to `K/S`. Measure at two concentrations (or with a known substrate) to separate K and S rather than lumping them into K/S.
- **Fit.** Given a target set of mixed colors and their measured spectra, solve a least-squares for the primary pigments' K and S that minimize reconstruction error. Useful for matching a specific artist's palette.

A minimal palette: **Titanium White, Carbon Black, Cadmium Yellow, Pyrrol Red, Phthalo Blue, Phthalo Green**. Six pigments cover a surprisingly wide subjective gamut.

## Runtime shape

Per brush sample where two pigmented colors meet:

1. Look up or upsample `R_a(λ)`, `R_b(λ)` from each RGB.
2. Convert each to `K/S_a(λ)`, `K/S_b(λ)`.
3. Derive concentrations of the primary pigments for each endpoint. (Alternatively, if your intermediate representation *is* pigment concentrations, skip 1–3.)
4. Blend concentrations: `c_mix = t · c_a + (1−t) · c_b`.
5. Reassemble `K_mix / S_mix` from the primaries' K and S tables under `c_mix`.
6. Invert K-M to get `R_mix(λ)`.
7. Integrate to XYZ, matrix to linear sRGB.

Steps 1 and 7 are the costly ones; cache both as LUTs if the brush hits the same colors repeatedly. Steps 3–5 are the interesting ones — and are exactly the part the Sochorová/Jamriška paper optimizes via a latent decomposition. See [latent-space.md](latent-space.md).

## GPU or CPU

- **GPU**: 7–10 bin spectral mixing is a straightforward WGSL pass — one texture sample per primary's `K/S`, one spectral accumulation loop. Fits in `aj-effects`.
- **CPU**: a worker on the rayon pool resolving full-quality pigment mixing for a stroke's final "resolve" pass while a cheap additive placeholder renders the in-flight stroke. Matches the latency strategy in `CLAUDE.md`.

Start on CPU for the resolve path; move to GPU if we measure we need it.

## Pitfalls

- **Negative `K/S`.** Numerical noise at the endpoints (R near 0 or 1) can drive `K/S` to large or negative values. Clamp `R` to `[ε, 1−ε]` before inverting.
- **Gamut excursions.** A spectral mix can fall outside sRGB; run it through the gamut mapping in `perceptual-color/gamut-mapping.md` before storing.
- **White pigment is special.** Adding titanium white does not just "lerp toward 1" — it changes both K and S and produces the subtle chalky quality real mixed-with-white paint has. Get the white pigment's KS right and everything else lines up.
- **Illuminant choice.** D65 by default (matches sRGB). Don't use D50 (print-world default) unless we add ICC / print targeting, which is a v1 non-goal.

## Sources

- [Kubelka–Munk theory — Wikipedia](https://en.wikipedia.org/wiki/Kubelka%E2%80%93Munk_theory)
- Meng, Simon, Hanika, Dachsbacher (2015). Physically Meaningful Rendering using Tristimulus Colours. *EGSR.*
- Jakob & Hanika (2019). A Low-Dimensional Function Space for Efficient Spectral Upsampling. *Eurographics.*
- Smits (1999). An RGB-to-Spectrum Conversion for Reflectances. *Journal of Graphics Tools.*
- Curtis, Anderson, Seims, Fleischer, Salesin (1997). Computer-Generated Watercolor. *SIGGRAPH.* — early digital K-M painting.
