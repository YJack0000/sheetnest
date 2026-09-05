//! DXF in and out (`dxf` feature).
//!
//! [`parse_dxf`] turns a drawing into [`crate::Part`]s; [`write_dxf`] turns
//! a nested layout back into a drawing for the cutter. Both are pure Rust
//! on top of the `dxf` crate.

mod read;
mod write;

pub use read::{ParsedFile, chain_segments, parse_dxf};
pub use write::{LAYER_CUT, LAYER_SHEET, SHEET_GAP_MM, sheet_offset, write_dxf};
