# art-junk

Cross-platform (iOS, Android, Windows, macOS, Linux, Web) GPU-accelerated drawing program in Rust. Retained vector scene graph (no raster layers), stylus-first input, designed for 120+ FPS drawing with the closest-to-paper latency feasible.

License: dual `MIT OR Apache-2.0`.

## Plan format (mandatory)

**Every plan — written, in a PR description, or produced via TodoWrite — must include sections on:**

1. **DRY** — what duplication is avoided or introduced; how existing code is reused or generalized.
2. **Standards & best practices** — at the code level (Rust idioms, cargo conventions, clippy-clean, formatted) *and* UX (accessibility, latency budget, affordances, discoverability).
3. **User experience / fitness for use** — how the change improves the end-user's experience of the drawing app, not just internal mechanics.
4. **Future-self ergonomics** — what makes this code easier to modify, debug, or extend later; what traps are avoided.

Not optional boilerplate. If a section is genuinely N/A for a small change, state that explicitly rather than omit the section.

## Tech stack

| Concern | Choice |
|---|---|
| GPU | `wgpu` (WebGPU required for web target; WebGL2 cannot host Vello) |
| Windowing / input | `winit` with platform-specific extensions where stylus coverage is thin |
| 2D rendering | `vello` + `kurbo` + `peniko` (Linebender ecosystem) |
| Text | `parley` + `swash` — deferred; v1 is labels only |
| UI chrome | `egui` at first pass (replace/theme later; drawing surface stays raw Vello) |
| Stylus / palm rejection | home-grown `aj-stylus` crate |
| Serialization | `serde` → CBOR binary (`.aj`) + RON ASCII (`.ajr`), converter CLI |
| Concurrency | `std::thread` + `crossbeam-channel` + `rayon` + `arc-swap` + `pollster`. **No `tokio`.** |
| Testing | `cargo test`, `insta` snapshots, headless wgpu render-to-texture for goldens, `dssim` perceptual diff |

## Workspace layout

```
crates/
  aj-core/     # data types, scene graph, serde schema — pure, no threading
  aj-engine/   # actor thread, Command/Event, snapshot publication, task pool
  aj-format/   # (de)serialization, I/O adapters
  aj-stylus/   # cross-platform stylus + palm-rejection abstraction
  aj-render/   # Vello integration, dirty-tile tracking, consumes SceneSnapshot
  aj-effects/  # scene-graph effect nodes + WGSL passes
  aj-app/      # main binary — wires engine + render + egui UI
tools/
  aj-convert/  # CBOR ⇄ RON CLI
```

Crate boundaries enforce engine-vs-renderer-vs-UI separation from day one.

## Architecture

**Actor engine.** `aj-engine` owns the authoritative `Document`. UI sends `Command`s; engine processes them in order, spawns workers for heavy tasks, publishes `ArcSwap<SceneSnapshot>` that the renderer reads lock-free. Workers are pure: they take immutable snapshots, produce candidate results, post them back as `Event`s. **Engine thread is the only writer** — sequentiality emerges from that, not from any special worker lane.

**Task pool.** One rayon pool (num_cpus) for all CPU-parallel work. Long tasks are cancellable by default; uninterruptible is opt-in for commit-phase work. Per-resource gates (`save_channel`, `export_channel`, …) enforce one-at-a-time semantics where needed; UI reads gates to disable conflicting commands.

**Status.** `Vec<TaskStatus>` in the snapshot; UI renders one row per task with progress + cancel, minimum one "Idle" row.

**Latency strategy.** Each stroke carries a render state: `Placeholder | Resolving(gen) | Full(gen, cache)`. New input samples render cheaply on the main path in <1ms; a worker resolves full brush texturing off-thread and swaps in. Where available (iOS / Windows / macOS) platform predicted-touch APIs lead the physical pen by one frame — progressive enhancement.

**Scene graph.** Slotmap-backed node tree with stable IDs. Effect nodes adopt their targets as children (so an effect subtree renders as a unit). Undo/redo is a diff log over the tree.

**Web degradation.** Same Command/Event plumbing on WASM; rayon collapses to inline execution for v1 (small tasks briefly freeze UI, big tasks show modal). Later milestone promotes to `wasm-bindgen-rayon` workers. Tasks are *not* pre-structured as yielding state machines — that's preemptive cost we may never need to pay.

## Architecture diagrams

The `architecture/` folder holds Mermaid diagrams that document the system visually: crate graph, data flow, threading model. These are the onboarding source-of-truth — stale diagrams are worse than none.

**Update rule**: any change that adds/removes a crate, introduces a new channel or event, changes the threading model, or changes crate boundaries MUST update the relevant `architecture/*.md` file in the same PR. Applies to human and LLM contributors alike.

## Non-goals (v1)

- Rich / publishing-tier text (labels only).
- Raster-layer painting.
- Native look per platform — custom UI everywhere, intentionally consistent across platforms.
- Research-grade perceptual rasterization — Vello's analytical-coverage AA is the target quality.
- ICC color management / HDR — linear-float working space, sRGB display.
- `tokio` or any full async runtime.

## Conventions

- **Comments**: default to none. Write only when the WHY is non-obvious (hidden constraint, workaround, surprising invariant). Never explain WHAT — names should do that.
- **Error handling**: `anyhow::Result` at application boundaries; typed errors (`thiserror`) inside library crates.
- **Main-thread budget**: ~4ms input+UI, ~4ms render of an 8.3ms (120Hz) frame. If unsure whether something fits, assume slow and route through the engine task pool.
- **Testing**: prefer unit tests + golden images over mocking. Golden images compare with `dssim` perceptual diff — **never pixel-equality**; AA differs across GPUs and CI will flake.
- **Dependencies**: all must be MIT/Apache-2.0-compatible. `cargo-deny` enforces this in CI.
- **License headers**: not required per-file. `LICENSE-MIT` and `LICENSE-APACHE` at repo root; workspace Cargo.toml declares `license = "MIT OR Apache-2.0"`.
- **Formatting / lints**: `cargo fmt` and `cargo clippy -- -D warnings` are CI gates.
- **Architecture diagrams**: keep `architecture/*.md` in sync with structural changes — see § Architecture diagrams.

## Build / test commands

Expected shape once scaffolding lands (Milestone 1):

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check
cargo run -p aj-app
```

Mobile (iOS/Android) and web (WASM) commands to be added as those milestones land.
