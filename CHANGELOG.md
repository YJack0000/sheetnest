# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Initial extraction of the nesting engine from the auto-flat application
  (August 2026) as a standalone crate.
- `Part::from_polygon` and `Part::from_contours` so parts can be built
  without going through DXF.
- `NestConfig::seed` for reproducible runs.
- `Hooks::should_stop` for cancellation; `Progress` struct for the
  per-generation callback; `StopReason` in `NestStats`.
- `NestSolution::sheet_width` / `sheet_height` and `render_config`, so
  renderers draw auto-width results at the width actually reached.
- Cargo features `dxf`, `svg`, `parallel`.
- `sheetnest-cli` (`sheetnest nest | validate | bench`).
- Python package `sheetnest` (PyO3, abi3 wheels) with GIL release, progress
  and cancellation hooks, Ctrl-C support.
- npm package `sheetnest` (wasm-bindgen; Node and browser via `exports`).
- The unseeded RNG seed comes from the clock and a counter instead of OS
  entropy, so the core builds for wasm32 without a `getrandom` backend.
