//! The in-place overlay's pure core (spec
//! `docs/superpowers/specs/2026-10-16-overlay-design.md`, slice 1, #808).
//! No Win32 here: everything is a function over OCR lines and numbers, so it
//! is unit-tested. Slice 2 (#809) adds the window and the first caller, and
//! removes the `allow(dead_code)` markers in this module.

#[cfg_attr(not(test), allow(dead_code))]
pub mod alpha;
#[cfg_attr(not(test), allow(dead_code))]
pub mod anchor;
#[cfg_attr(not(test), allow(dead_code))]
pub mod layout;
#[cfg_attr(not(test), allow(dead_code))]
pub mod state;
