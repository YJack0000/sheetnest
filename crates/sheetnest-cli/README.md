# sheetnest-cli

Command-line 2D nesting for sheet cutting. Reads DXF drawings, works out where
every part goes on the stock, and writes a cutting file for the laser, plasma
or router.

The packing is a no-fit-polygon placer driven by a genetic algorithm — the same
family as SVGnest — from the [`sheetnest`](https://crates.io/crates/sheetnest)
crate. Arcs stay analytic all the way to the output, so the machine gets real
`ARC` entities rather than a thousand tiny chords, and optional micro-joints
keep cut parts tacked into the sheet instead of tipping into the head.

All lengths are millimetres, all angles degrees.

## Install

```sh
cargo install sheetnest-cli
```

That puts a `sheetnest` binary on your path.

## `sheetnest nest` — pack the job

Name each drawing once, with the number of copies after a colon:

```sh
sheetnest nest bracket.dxf:12 gusset.dxf:4 plate.dxf -o job.dxf
```

```
placed          : 17/17
sheets used     : 1
used width      : 1642.0 mm
utilization     : 68.4%
strip util      : 76.1%
rounds          : 612
elapsed         : 8104 ms
stopped because : stopped improving
```

A different sheet, wider kerf clearance and micro-joints, with a preview
picture alongside the cutting file:

```sh
sheetnest nest bracket.dxf:12 gusset.dxf:4 \
  --sheet 2500x1250 --spacing 4 --margin 10 \
  --tabs --tab-width 0.4 --tab-spacing 200 \
  -o job.dxf --svg job.svg
```

Cutting from a coil instead of a plate — `--auto-width` lets the nester run as
far along the strip as it likes and tells you where to cut. Only the height in
`--sheet` matters here; the width is ignored:

```sh
sheetnest nest parts.dxf:40 --auto-width --sheet 1829x1250 -o coil.dxf
```

Same layout twice, for a repeat job or a regression test:

```sh
sheetnest nest parts.dxf:40 --seed 42 --stale-generations 300 -o job.dxf
```

Machine-readable output for a script or a shop-floor app (this prints the whole
solution — every placement, plus the statistics — and skips the summary):

```sh
sheetnest nest parts.dxf:40 --json --quiet > layout.json
```

Settings you use every day belong in a file rather than in your shell history.
The keys are the camelCase form of the nest options; anything you also pass on
the command line wins over the file:

```jsonc
// shop.json
{
  "sheetWidth": 2500,
  "sheetHeight": 1250,
  "spacing": 4,
  "margin": 10,
  "rotationMode": "orthogonal",
  "timeLimitMs": 30000,
  "tabs": { "enabled": true, "width": 0.4, "maxSpacing": 200 }
}
```

```sh
sheetnest nest parts.dxf:40 --config shop.json -o job.dxf
```

Progress ticks on stderr, so piping stdout stays clean. `--quiet` silences
everything but errors. A part too big for the sheet is reported and left out;
the run still succeeds, so check the `placed` line before you cut.

Run `sheetnest nest --help` for the full list of options.

## `sheetnest validate` — measure the finished layout

Reads a nested DXF back the way a cutter would and checks it against the
physical rules, without asking the nester whether it did a good job: nothing
overlaps, every part sits inside exactly one sheet with the margin respected,
and no two parts are closer than the spacing.

```sh
sheetnest validate job.dxf --spacing 4 --margin 10 --sheet 2500x1250 --expect 17
```

```
micro-joint gaps detected: 0
CUT entities: 214 segs -> 48 closed loops, 0 open chains
top-level contours (part instances): 17
--------------------------------------------------------------
VALIDATION PASSED (worst overlap 0.0000 mm^2)
```

Exits non-zero, listing each violation, when something is wrong.

## `sheetnest bench` — compare runs

Nests every `.dxf` in a folder as one job and prints the metrics, so two builds
or two sets of cutting parameters can be weighed against the same pile of
parts.

```sh
sheetnest bench ./parts 3 --time-limit-ms 20000 --rotation free
```

```
bench: 5 parts x qty 3 = 15 instances | sheet 1829x914 | Free | budget 20000ms
--------------------------------------------------------------
placed          : 15/15
sheets used     : 1
used width      : 1204.0 mm
utilization     : 61.7%
strip util      : 79.5%
rounds          : 743
wall time       : 19140 ms
```

Compare `used width` and `strip util` between runs; do not compare them against
another nesting tool's numbers, which are measured differently.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| 0 | Ran to completion. For `nest`, some parts may still have been left out — read the summary. |
| 1 | `validate` found a violation. |
| 2 | Bad arguments, an unreadable drawing, or a write that failed. |

## License

MIT OR Apache-2.0, at your option.
