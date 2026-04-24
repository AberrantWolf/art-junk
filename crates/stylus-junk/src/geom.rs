//! Minimal geometry types for the public input API.
//!
//! The crate's public surface uses these rather than `kurbo::Point` /
//! `kurbo::Size` so consumers don't have to adopt kurbo to use stylus-junk.
//! Field layout is identical to kurbo's, so the `feature = "kurbo"` interop
//! module's `From` impls are literal field copies.

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const ZERO: Self = Self { width: 0.0, height: 0.0 };

    #[must_use]
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}
