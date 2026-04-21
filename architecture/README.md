# Architecture diagrams

Mermaid diagrams documenting art-junk's structure.

- [crates.md](crates.md) — crate dependency graph
- [dataflow.md](dataflow.md) — input → command → engine → snapshot → render → screen
- [threading.md](threading.md) — main thread / engine thread / worker pool

## Update rule

When you add or rename a crate, introduce a new channel or event, change the threading model, or change crate boundaries, update the relevant diagram in the same change. See [../CLAUDE.md](../CLAUDE.md) § Architecture diagrams. Applies to human and LLM contributors alike.
