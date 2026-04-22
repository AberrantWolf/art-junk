//! Brush-rendering module. Today one brush type (ribbon); future types
//! (stamp, textured, airbrush) get sibling modules here and the match on
//! brush kind expands at the call site in `crate::lib`. If this module
//! grows into its own crate (`aj-brush`), the move is mechanical.

pub(crate) mod ribbon;

pub(crate) use ribbon::tessellate_stroke;
