# cosmic-gaze

Gaze-based pointing/scrolling/clicking for COSMIC (Wayland). Read `DESIGN.md` for the
why, `PLAN.md` for the Phase 0 build plan and per-crate contracts, `PLAN-ET5.md` for the
tracker provider and its calibration, and `PLAN-UX.md` for the overlay, daemon and applet.

## Code style
Follow `(private notes)` exactly (column-aligned fields/args, `// --- X ---`
section headers, explicit struct field syntax, docs on every item, edition 2024, no
`mod.rs`). Verification and scope rules: `(private notes)`.

## Verification
- `cargo build --workspace` and `cargo test --workspace` must pass with zero warnings.
- `cargo clippy --workspace` clean.
- Binaries that need a live compositor, a device node, or a model file: state what was and
  was not run, and how to run it manually.

## Conventions
- Units in names: `_px` logical pixels, `_mm`, `_deg`, `_s`. Angular thresholds are always
  degrees and seconds, never samples or pixels (providers run at 30 to 133 Hz).
- All element boxes are in global logical pixels (`gaze_core::Rect`).
- Shared types live in `gaze-core`; do not duplicate them. Extend `gaze-core` only when a
  type is needed by more than one crate, and say so in your report.
- Every crate ships a small CLI under `src/bin/` for manual testing when it touches the
  outside world.
- Models and screenshots are gitignored; document where they come from in the crate README.
