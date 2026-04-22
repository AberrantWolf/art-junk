# 🎨 art-junk

> A cross-platform, GPU-accelerated, stylus-first drawing program in Rust.

Built around a **retained vector scene graph** (no raster layers), targeting **120+ FPS** drawing with the closest-to-paper latency feasible.

---

## ✨ What it's for

art-junk aims to be a fast, quiet, focused drawing surface — one that stays out of your way and respects the pen. Not a Photoshop clone, not a PDF annotator: a place to draw.

- 🖊️ **Stylus-first** — pressure, tilt, predicted touch where the platform offers it, honest palm rejection.
- ⚡ **Fast by default** — actor-style engine, lock-free snapshots to the renderer, rayon for heavy work.
- 🧭 **Vector native** — strokes are geometry, not pixels. Zoom in forever. "100%" means real-world physical size.
- 🪟 **Same app everywhere** — one consistent UI across desktop, tablet, and web.

## 🖥️ Target platforms

| Desktop | Mobile | Web |
| :-: | :-: | :-: |
| macOS · Windows · Linux | iOS · Android | WebGPU browsers |

## 🧱 Tech stack (at a glance)

```
      Input              Engine                Render             UI
  ┌──────────┐       ┌──────────┐          ┌──────────┐      ┌──────────┐
  │  winit   │──────▶│ actor +  │─ArcSwap─▶│  vello   │      │   egui   │
  │ aj-stylus│       │  rayon   │ snapshot │  + wgpu  │      │          │
  └──────────┘       └──────────┘          └──────────┘      └──────────┘
```

Rust · `wgpu` · `vello` + `kurbo` + `peniko` · `winit` · `egui` · `serde` (CBOR / RON)

More detail — including crate graph, data flow, and threading model — lives in [`architecture/`](architecture/).

---

## 🚧 Status

Early, pre-alpha, hobby-paced. Not ready for real use yet. There's no release, no install story, no stability promise. Watch the repo if you're curious; come back later if you want to draw something.

---

## 📜 License

Dual-licensed under either of

- **Apache License, Version 2.0** — see [`LICENSE-APACHE`](LICENSE-APACHE)
- **MIT License** — see [`LICENSE-MIT`](LICENSE-MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
