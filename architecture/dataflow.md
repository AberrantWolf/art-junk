# Input → render data flow

```mermaid
sequenceDiagram
  participant UI as winit event loop (aj-app)
  participant Chr as egui chrome (aj-app)
  participant Act as ActionTable (aj-app)
  participant VP as Viewport (aj-app)
  participant Eng as aj-engine actor thread
  participant His as History (undo/redo stacks)
  participant Snap as ArcSwap&lt;AppSnapshot&gt;
  participant Ren as aj-render (Vello)

  Note over UI,Chr: pointer / key events first routed through egui
  UI->>Chr: on_window_event(event)
  UI->>Act: key event (if egui didn't consume)
  Act->>Eng: Command::Undo / Command::Redo
  UI->>VP: screen_to_world(cursor_physical_px) → world_pt
  UI->>Eng: Command::BeginStroke / AddSample / EndStroke (world-space pt)
  UI->>Eng: Command::SetPageSize / SetShowBounds / SetClipToBounds
  UI->>VP: mutate on MouseWheel / PinchGesture / ViewAction
  Eng->>His: record inverse on EndStroke; pop / push on Undo / Redo
  Eng->>Snap: store(Arc::new(AppSnapshot { scene: SceneSnapshot { page, strokes }, history }))
  UI->>UI: window.request_redraw()
  UI->>VP: to_affine(dpi_scale) → world_to_screen: Affine
  UI->>Ren: render(&snapshot.scene, world_to_screen, surface_texture)
  UI->>Chr: paint(surface_texture, full_output)   # egui overlay, LoadOp::Load
  UI->>UI: frame.present()
```

## Notes

- UI never mutates `DocumentState` directly — every change is a `Command` on a channel.
- The engine drains commands (recv + try_recv) and publishes a single `AppSnapshot` per
  batch, so burst input does not cause snapshot thrash.
- `AppSnapshot` bundles the renderer-facing `SceneSnapshot` (arc'd, so cheap to clone)
  with a `HistoryStatus { can_undo, can_redo }` so UI can enable/disable menu entries
  without reaching into engine internals. Renderer ignores `history`; UI reads
  `scene.page` for toggle checkmarks but ignores `scene.strokes`.
- `SceneSnapshot` is `{ page: Page, strokes: Vec<Stroke> }`. Page state rides the
  same single ArcSwap publication as strokes so the renderer reads both from a
  consistent view — no parallel channel for page mutations.
- Page mutations (`SetPageSize` / `SetShowBounds` / `SetClipToBounds`) are Commands
  but not `Edit`s: they bypass the history stack (non-undoable in v1, TODO noted).
- `Viewport` (pan / zoom state) lives entirely in `aj-app`. View state is not part
  of the document and does not ride the Command channel; it's mutated locally on
  mouse-wheel, pinch, and keyboard shortcuts. The engine only ever sees world-space
  (document-pt) coordinates — the app converts cursor physical-pixels → CSS px →
  points → world via `Viewport::screen_to_world` before dispatching stroke commands,
  and calls `Viewport::to_affine` each frame to hand the renderer one combined
  world-to-physical-pixels transform (`user_zoom × CSS_PER_PT × dpi_scale`).
- Mid-stroke zoom / pan is allowed: stored stroke samples are in world space, so
  a view change only affects the transform — the stroke itself remains contiguous.
- `History` stores the *inverse* of each applied forward edit on `past`, so committing
  a future edit that destroys data (e.g. `RemoveStroke`) captures the payload at apply
  time. `Undo` pops from `past`, applies, and pushes the resulting forward edit onto
  `future`; `Redo` is symmetric.
- A fresh commit after undo truncates `future` (standard tree → linear history).
- `Undo` / `Redo` are no-ops while a stroke is mid-drag (`DocumentState::has_active_stroke`).
- `EndStroke` is the commit point: active stroke moves into the strokes vec and one
  `Edit::AddStroke` lands on history.
- The renderer reads `ArcSwap::load_full()` lock-free; the engine can publish
  concurrently with no coordination.
- egui chrome shares the surface texture with Vello via two-submit overlay:
  Vello's `render_to_surface` submits first; egui-wgpu's pass uses `LoadOp::Load`
  on the same surface view so chrome overlays the drawing.
```
