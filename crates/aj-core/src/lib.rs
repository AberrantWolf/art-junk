//! Scene-graph data types and domain model for art-junk.

use std::sync::atomic::{AtomicU64, Ordering};

pub use kurbo::Point;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StrokeId(pub u64);

impl StrokeId {
    #[must_use]
    pub fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Clone)]
pub struct Stroke {
    pub id: StrokeId,
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, Default)]
pub struct SceneSnapshot {
    pub strokes: Vec<Stroke>,
}
