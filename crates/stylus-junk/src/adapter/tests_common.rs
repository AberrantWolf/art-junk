//! Test fixtures shared between every platform's adapter-seam tests.
//! Each platform-specific test module (`mac::tests`, `wayland::tests`, …)
//! pulls these in via `use super::super::tests_common::*;` (or through the
//! `pub(crate) use` re-export in `mod.rs`).

use crate::{Sample, ToolCaps};

use super::StylusAdapter;
use crate::{Phase, StylusEvent};

pub(crate) fn adapter() -> StylusAdapter {
    // Don't pre-seed any platform's clock anchor: the real flows populate
    // them on first handle_<platform>_raw, and exercising that anchoring
    // path is part of what the tests cover.
    StylusAdapter::new()
}

pub(crate) fn drained(a: &mut StylusAdapter) -> Vec<StylusEvent> {
    a.drain().collect()
}

/// Destructure an event expected to be a `Sample` variant; panic otherwise.
/// Keeps per-test noise down now that `StylusEvent` is an enum with
/// additional variants (`Revise`, `PencilInteraction`).
pub(crate) fn expect_sample(ev: &StylusEvent) -> (&Sample, Phase, ToolCaps) {
    match ev {
        StylusEvent::Sample { sample, phase, caps } => (sample, *phase, *caps),
        other => panic!("expected StylusEvent::Sample, got {other:?}"),
    }
}
