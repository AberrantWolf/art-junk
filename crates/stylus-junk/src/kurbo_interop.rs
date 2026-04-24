//! `From`/`Into` bridges between this crate's `Point`/`Size` and kurbo's.
//!
//! Gated on `feature = "kurbo"`. Field layouts are identical, so these are
//! trivial field copies — the feature exists so consumers who don't want
//! kurbo in their dep graph aren't forced to pull it in.

use crate::geom::{Point, Size};

impl From<kurbo::Point> for Point {
    fn from(p: kurbo::Point) -> Self {
        Self { x: p.x, y: p.y }
    }
}

impl From<Point> for kurbo::Point {
    fn from(p: Point) -> Self {
        Self { x: p.x, y: p.y }
    }
}

impl From<kurbo::Size> for Size {
    fn from(s: kurbo::Size) -> Self {
        Self { width: s.width, height: s.height }
    }
}

impl From<Size> for kurbo::Size {
    fn from(s: Size) -> Self {
        Self { width: s.width, height: s.height }
    }
}
