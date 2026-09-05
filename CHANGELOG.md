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
