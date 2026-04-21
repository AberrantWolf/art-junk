# Input → render data flow

```mermaid
sequenceDiagram
  participant Win as winit event loop (aj-app)
  participant Eng as aj-engine actor thread
  participant Snap as ArcSwap&lt;SceneSnapshot&gt;
  participant Ren as aj-render (Vello)

  Win->>Eng: Command::BeginStroke{id, point}
  Win->>Eng: Command::AddSample{id, point} per CursorMoved
  Eng->>Snap: store(Arc::new(new snapshot))
  Win->>Win: window.request_redraw()
  Win->>Ren: render(snapshot.load_full(), surface_texture)
  Ren->>Win: frame.present()
  Win->>Eng: Command::EndStroke{id}
```

## Notes

- UI never mutates Document state directly — every change is a `Command` on a channel.
- The engine drains commands (recv + try_recv) and publishes a single snapshot per batch, so burst input does not cause snapshot thrash.
- The renderer reads `ArcSwap::load_full()` which is lock-free; the engine can be publishing a new snapshot concurrently with no coordination.
- `EndStroke` does not itself change rendered output (in M2) — it just closes the input phase for that stroke ID.
