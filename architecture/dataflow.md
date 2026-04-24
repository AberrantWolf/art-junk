# Input → render data flow

```mermaid
sequenceDiagram
  participant UI as winit event loop (aj-app)
  participant Chr as egui chrome (aj-app)
  participant Sty as StylusAdapter (stylus-junk)
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
  UI->>Sty: on_window_event(event) (unless chrome owns pointer & no active stroke)
  Note over Sty: on macOS an NSEvent local monitor ALSO pushes<br/>richer pen samples (pressure/tilt/twist) into Sty<br/>before winit dispatches; Sty.active_pen_pointer<br/>then suppresses the duplicate mouse sample.
  Sty-->>UI: drain() → StylusEvent::Sample { sample, phase, caps }<br/>   or StylusEvent::Revise { pointer_id, update_index, revision }
  UI->>VP: screen_to_world(sample.position) → world-space sample
  UI->>Eng: Command::BeginStroke { id, sample, caps, brush } / AddSample / EndStroke<br/>   or Command::ReviseSample { stroke_id, update_index, revision }
  UI->>Eng: Command::SetPageSize / SetShowBounds / SetClipToBounds
  UI->>Eng: Command::SetBrushMaxWidth / SetBrushMinWidth / SetBrushMinRatio (sliders + [ / ] / Alt+[ / Alt+])
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

- `StylusAdapter` is the single translation point from platform events to the
  app's internal `StylusEvent` stream. It owns `PointerId` allocation, mouse /
  touch / pen state, and hosts platform backends through a `pub(crate)` seam
  (`handle_mac_raw` / `handle_mac_proximity` / `on_focus_lost`). macOS pen data
  arrives via the `MacTabletBackend` in `stylus-junk::macos_tablet`, which
  installs an `NSEvent` local monitor and an `NSApplicationDidResignActive`
  observer; future iOS / Windows / Android backends will plug into the same
  seam. Adapter output is screen-space — world-space conversion stays in
  `aj-app` because the viewport lives there.
- **Pen/mouse deduplication on macOS**: when a Wacom-style tablet is in use,
  macOS delivers pen input both as `NSEventTypeTabletPoint` (native) *and* as
  regular mouse events with `subtype == .tabletPoint`. The `NSEvent` local
  monitor runs before `sendEvent:` dispatches the event to the window, so our
  richer pen sample is queued first and `active_pen_pointer` is set. Winit's
  subsequent dispatch lands the duplicate mouse event in `on_window_event`,
  which sees the flag and drops it. Egui still receives the winit mouse
  stream, so menu interaction via pen continues to work.
- **Estimated → Revise → Committed**: a pen-down on macOS often reports
  under-settled pressure on the first sample. The adapter emits that Down
  sample with `SampleClass::Estimated { update_index }` and records the
  pending index. When the next platform sample arrives (native
  `NSTabletPoint` or a follow-up `LeftMouseDragged`), the adapter emits a
  `StylusEvent::Revise` carrying the refined fields. The engine's
  `Command::ReviseSample` mutates the earlier sample in-place (active stroke
  first, last-committed stroke as a race-rescue) and promotes its class to
  `Committed`. No `Edit` is produced — revisions are pre-commit; by the time
  `EndStroke` lands, history stores the final values.
- **Focus-loss cancel**: observing `NSApplicationDidResignActive`, the backend
  calls `adapter.on_focus_lost()` which emits a `Cancel` phase for every
  active pointer (mouse, pen, touch). This prevents half-drawn strokes from
  being stranded when the user Cmd-Tabs mid-drag — macOS would route the
  eventual mouse-up to the new key app, and without the synthetic cancel our
  stroke would never terminate.
- **Borrow discipline**: the adapter is held in `Rc<RefCell<_>>` so the
  NSEvent monitor callback (running on the main thread, same as winit's
  event loop) can mutate it. The app's `route_input` borrows `stylus` in a
  narrow scope so the monitor's `try_borrow_mut` never collides; on the
  unlikely collision, the sample is logged and dropped rather than panicking.
- Chrome-owned input gating: if egui consumes an event or the pointer sits over
  chrome, `aj-app` declines to forward the event to the adapter **unless a
  stroke is already in progress** (`adapter.is_tracking_pointer()`). This
  preserves today's invariant that a stroke begun on the canvas runs to
  completion even if the cursor crosses the menu bar mid-drag.
- `Sample` is mandatory-fields-with-defaults for things every platform can
  supply (position, timestamp, pressure, tool, buttons, pointer_id) and
  `Option<T>` for fields that are genuinely platform-dependent (tilt, twist,
  tangential pressure, distance, contact size). The per-stroke `ToolCaps`
  bitflags let UI hide pressure-sensitive controls before the first sample
  arrives, avoiding the "0 vs missing" ambiguity that plagues Web and Windows
  sentinel conventions.
- `SampleClass` carries `Committed | Predicted | Estimated { update_index }`
  from day one. No backend produces `Predicted` or `Estimated` samples in
  Milestone 1, but iOS PencilKit's late-arriving estimation-resolution updates
  can be landed later by walking the active stroke and replacing samples
  matching `update_index` — no schema migration needed.
- **Brush: live vs. frozen.** `DocumentState::brush` is the live document
  brush — what a fresh stroke will be stamped with on the next `BeginStroke`.
  `Stroke::brush` is the snapshot taken at `BeginStroke` time and frozen for
  that stroke's lifetime. The renderer reads the latter; the brush panel
  reads `SceneSnapshot::brush` (the live one) to drive sliders. Mid-stroke
  slider changes don't affect the in-progress stroke — only the next one.
- **Ratio preservation in the engine.** `set_brush_max_width` computes the
  current `min/max` ratio and applies it to the new max so the user-perceived
  dynamics ratio stays constant. The ratio is the primary cognitive state;
  `min_width` (stored absolute) is effectively a cache of `ratio * max_width`.
  Sliders and `[` / `]` both funnel through `Command::SetBrushMaxWidth`;
  `Command::SetBrushMinRatio` handles the min-ratio slider and `Alt+[` /
  `Alt+]`. No floor on min — vector rendering handles sub-pixel widths.
- **Ribbon tessellation.** `aj-render::brush::ribbon::tessellate_stroke`
  fits a centripetal Catmull-Rom spline through sample positions, arc-length-
  tessellates each `CubicBez` at ~1 physical px, emits a filled polygon
  (left rail forward, right rail reversed) with round caps at each endpoint.
  Runs every frame — caching is a future optimization pass.
- **Shortcut registry dispatches on `(logical, physical, modifiers)`.**
  Alphabetic bindings (`Cmd+Z` etc.) match on `Key::Character` for
  keyboard-layout robustness. Non-alphabetic bindings like `[` / `]` /
  `Alt+[` match on `PhysicalKey::Code(KeyCode::*)` because on macOS Alt
  composes replacement characters that would bypass logical-key matching.
- UI never mutates `DocumentState` directly — every change is a `Command` on a channel.
- The engine drains commands (recv + try_recv) and publishes a single `AppSnapshot` per
  batch, so burst input does not cause snapshot thrash.
- `AppSnapshot` bundles the renderer-facing `SceneSnapshot` (arc'd, so cheap to clone)
  with a `HistoryStatus { can_undo, can_redo }` so UI can enable/disable menu entries
  without reaching into engine internals. Renderer ignores `history`; UI reads
  `scene.page` for toggle checkmarks but ignores `scene.strokes`.
- `SceneSnapshot` is `{ page: Page, strokes: Vec<Stroke> }`. A `Stroke` is
  `{ id, samples: Vec<Sample>, caps: ToolCaps, brush: BrushParams }`; the
  renderer currently reads only `sample.position`, but `pressure` / `tilt` /
  `brush` are carried end-to-end so variable-width rendering (a later
  milestone) doesn't need a data-shape change. Page state rides the same
  single ArcSwap publication as strokes so the renderer reads both from a
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
