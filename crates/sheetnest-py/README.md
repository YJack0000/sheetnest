# sheetnest

2D nesting for sheet cutting: pack parts onto rectangular stock with as little
waste as possible.

The optimizer is a no-fit-polygon (NFP) placer driven by a genetic algorithm
over placement order and rotation — the same family as SVGnest, implemented
natively in Rust on top of Clipper2. Arcs stay analytic all the way to the
output, so a laser or plasma cutter gets true `ARC` entities, and optional
micro-joints (tabs) keep cut parts from tipping into the head.

All lengths are millimeters, all angles degrees, y is up.

## Install

```bash
pip install sheetnest
```

Wheels are abi3 and cover CPython 3.9+. There is nothing to compile and no
runtime dependency.

## Example

```python
from pathlib import Path
from sheetnest import NestConfig, Part, nest

parts, warnings = Part.from_dxf(Path("bracket.dxf").read_bytes(), "bracket", quantity=12)
parts.append(Part.from_polygon("plate", 4, [(0, 0), (120, 0), (120, 80), (0, 80)]))

config = NestConfig(sheet_width=1829, sheet_height=914, spacing=2, margin=5, seed=1)
solution = nest(parts, config, on_progress=lambda p: print(p.generation, p.best_utilization))

print(f"{solution.stats.placed}/{solution.stats.total} placed "
      f"on {solution.stats.sheets_used} sheet(s), "
      f"{solution.stats.used_width:.0f}mm of stock consumed")
for p in solution.placements:
    print(f"{p.part_name} #{p.instance} sheet {p.sheet} @ ({p.dx:.1f}, {p.dy:.1f}) rot {p.rotation_deg}")

Path("nested.dxf").write_bytes(solution.to_dxf())
Path("nested.svg").write_text(solution.to_svg_all())
```

## API

| Object | What it is |
| --- | --- |
| `Part.from_polygon(name, quantity, outer, holes=[])` | A part from plain vertex rings. Either winding order, closing vertex optional. |
| `Part.from_dxf(data, name, quantity=1, curve_tolerance=0.25)` | `(parts, warnings)` from the bytes of a DXF file. |
| `NestConfig(**kwargs)` / `.from_dict(d)` / `.to_dict()` | Run settings; see the table below. |
| `nest(parts, config=None, *, on_progress=None, should_stop=None)` | Runs the optimizer, returns a `Solution`. |
| `Solution` | `.placements`, `.stats`, `.sheet_width`, `.sheet_height`, `.warnings`, `.to_dict()`, `.to_dxf()`, `.to_svg(sheet=0)`, `.to_svg_all()` |
| `Placement` | `part_id` (index into `parts`), `part_name`, `instance`, `sheet`, `rotation_deg`, `dx`, `dy` |
| `Stats` | `stop_reason`, `sheets_used`, `used_width`, `utilization`, `strip_utilization`, `generations`, `elapsed_ms`, `placed`, `total` |
| `Progress` | `generation`, `best_fitness`, `best_utilization`, `elapsed_ms` |

A placement means: rotate the part's local geometry by `rotation_deg` about the
origin, then translate by `(dx, dy)`. Part geometry is normalized so the outer
bounding box's min corner sits at `(0, 0)`.

`stop_reason` is one of `"time_limit"`, `"stale"`, `"cancelled"`, `"empty"`.

### Config fields

| Field | Default | Meaning |
| --- | --- | --- |
| `sheet_width` | `1829.0` | Usable sheet length along X, mm. Ignored when `auto_width` is set. |
| `sheet_height` | `914.0` | Usable sheet height along Y, mm. |
| `auto_width` | `False` | Treat the stock as a strip of unbounded length. A second sheet is never opened and `stats.used_width` is the answer — how far along the coil to cut. |
| `spacing` | `2.0` | Minimum gap between parts, mm. |
| `margin` | `5.0` | Minimum gap between parts and the sheet edge, mm. |
| `rotation_mode` | `"orthogonal"` | `"orthogonal"` (0/90/180/270) or `"free"` (multiples of `rotation_step_deg`). |
| `rotation_step_deg` | `15.0` | Rotation granularity in `"free"` mode, degrees. |
| `curve_tolerance` | `0.25` | Max chord error when linearizing curves for the nesting math, mm. |
| `time_limit_ms` | `20000` | Hard wall-clock stop. |
| `population` | `15` | GA population size. |
| `mutation_rate` | `0.10` | Per-gene mutation probability, 0.0–1.0. |
| `stale_generations` | `600` | Generations without improvement before giving up early. Raising it trades latency for layout quality. |
| `seed` | `None` | RNG seed. See determinism below. |
| `tabs` | disabled | Micro-joints; a `TabConfig` or a dict. |

`tabs` fields: `enabled` (`False`), `width` (`0.3` mm gap left uncut),
`max_spacing` (`250.0` mm between adjacent tabs), `min_per_contour` (`2`),
`corner_clearance` (`3.0` mm), `min_hole_size` (`40.0` mm — smaller holes get
no tabs).

`NestConfig.from_dict` accepts camelCase or snake_case keys and fills the rest
from the defaults; `to_dict` always emits snake_case, as does
`Solution.to_dict`.

### Determinism

With a `seed`, the same input produces the same layout **whenever the run ends
on `stale_generations` rather than on the wall clock** — a time-based stop cuts
the search at a machine-dependent generation. For reproducible output, set a
seed and a `stale_generations` low enough that the run finishes well inside
`time_limit_ms`, then check `solution.stats.stop_reason == "stale"`.

## GIL, threads, and Ctrl-C

`nest()` releases the GIL for the whole run, so other Python threads keep
running and several nests can proceed in parallel from a thread pool. The
engine itself also uses all cores internally (rayon).

The two hooks are called once per generation, from the thread that called
`nest()`, with the GIL re-acquired:

- `on_progress(progress)` — a `Progress` snapshot. If it raises, the run stops
  and the exception is re-raised out of `nest()`.
- `should_stop() -> bool` — return `True` to stop early; the result comes back
  with `stats.stop_reason == "cancelled"` and the best layout found so far.

Ctrl-C is checked once per generation whether or not you pass a hook, so a long
run interrupts promptly and raises `KeyboardInterrupt`. The granularity is one
generation: a single very large generation still has to finish first.

## License

MIT OR Apache-2.0, at your option.
