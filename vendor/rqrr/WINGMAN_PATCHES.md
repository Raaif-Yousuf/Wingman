# Wingman patches to rqrr 0.11.0

This directory is `rqrr` 0.11.0 from crates.io (https://github.com/WanzenBug/rqrr),
licensed `(MIT OR Apache-2.0) AND ISC` (see `LICENSE-MIT`, `LICENSE-APACHE`,
`LICENSE-ISC`), with the changes below. Cargo uses it through
`[patch.crates-io]` in the root `Cargo.toml`. Every change is marked
`// wingman patch:` in the source. Nothing else was touched.

Why: Wingman scans whatever is on the screen, and a panic aborts the release
exe. Upstream asserts and loops on input it should reject (issues #528, #537,
#616).

MEASURED 2026-10-16: on a 1920x1080 frame of 1500 finder-like squares at
random places over speckle noise, upstream `detect_grids` panics inside
`Perspective::map` (debug build). The same frame now returns.

| File | Change | Why |
|---|---|---|
| `src/geometry.rs` | `Perspective::map` clamps its result to +/-8192 (NaN becomes 0) instead of four `assert!`s | A near-zero perspective denominator sends the point to infinity and the assert aborted. The clamp also keeps the callers' `i32` products from overflowing. |
| `src/identify/grid.rs` | `rotate_capstone` returns `Option`, uses wrapping arithmetic, and no longer calls `expect`; `from_group` propagates `None` | `Perspective::create` can fail after the corners are rotated. The group is skipped. |
| `src/identify/grid.rs` | `measure_timing_pattern` returns `None` when no timing transitions are found, instead of `assert!(scan >= 1)` | A flat timing pattern is not a code. |
| `src/identify/grid.rs` | `find_alignment_pattern`: wrapping arithmetic for the size estimate, which is capped at 65536 | A huge estimate made the spiral search run for billions of steps. |
| `src/detect.rs` | `capstones_from_image` stops at `MAX_CAPSTONES` (200) | Grouping is quadratic in the capstone count and copies the whole image per candidate group: 300 shapes took 3 s, 600 took 6 s in release. |

Not changed, and why: the `assert!`s in `src/decode.rs` guard internal
invariants of the Reed-Solomon and payload code, which upstream fuzzes. A
crowded-frame fuzz of 30000 frames did not reach them.

Upstream text: see the issue drafted for the rqrr project. When upstream
carries equivalent fixes, drop this directory and the `[patch.crates-io]`
entry.
