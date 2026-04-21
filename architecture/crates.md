# Crate dependency graph

```mermaid
graph TD
  app[aj-app] --> engine[aj-engine]
  app --> render[aj-render]
  app --> core[aj-core]
  engine --> core
  render --> core
  format[aj-format] --> core
  stylus[aj-stylus] --> core
  effects[aj-effects] --> core
  effects --> render
  convert["tools/aj-convert"] --> format
```

## Notes

- `aj-core` is the root data-type crate: pure types, no threading, no GPU. Everything depends on it.
- `aj-engine` is the only writer of `Document` state; `aj-render` and `aj-app` are read-only consumers of `SceneSnapshot` it publishes.
- `aj-app` is the wiring crate — it depends on everything else and produces the binary.
- `tools/aj-convert` is outside `crates/` so it never ships inside the app binary.
