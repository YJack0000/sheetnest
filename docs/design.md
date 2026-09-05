# How sheetnest works

This document walks through what happens between a batch of DXF parts and
a nested layout: what each step does, why it is designed that way, and
what we measured on real production data. It is written for the person
who wants to change the engine, and especially for the person who wants
to "tune the parameters to get better results".

## The one-paragraph version

sheetnest is a **no-fit polygon (NFP) + genetic algorithm** nester, the
same family as SVGnest, implemented natively in Rust with arcs kept in
analytic form from input to output. The genetic algorithm decides the
*order* in which parts are placed and their rotation; a deterministic
greedy decoder does the actual placing. The GA searches over orderings,
never over coordinates.

> **The single most important fact:** the objective minimizes the length
> of stock consumed along X, not area utilization. The sheet height is
> fixed; what the shop wants is to cut the coil as short as possible.
> When you see "utilization" anywhere, remember it is a report figure,
> not the quantity being optimized.

## Data flow

```
DXF bytes ──▶ dxf::parse_dxf ──▶ Part[]
                                   │
                                   ▼
                          place::prep_parts        simplify + inflate rings
                                   │
              ┌────────────────────┴────────────────────┐
              │  ga::run_nest                            │
              │    population of (order, rotation)       │
              │        │                                 │
              │        ▼                                 │
              │  place::decode  ◀── deterministic ───┐   │
              │        │                             │   │
              │        ▼                             │   │
              │     fitness ──▶ select / crossover ──┘   │
              └────────────────────┬────────────────────┘
                                   ▼
                            NestSolution
                                   │
                    ┌──────────────┴──────────────┐
                    ▼                             ▼
             dxf::write_dxf                  svg::sheet_svg
           (tabs::apply_tabs applied per part, in local coordinates)
```

Search and decode form a loop: the GA proposes candidate
(order + rotation) individuals, the decoder turns each into a concrete
layout and a score, and the score drives the next generation. The decoder
is deterministic, so the same genes always produce the same layout, which
is what makes it safe to evaluate the population in parallel.

## Step by step

### 1. DXF parsing (`dxf/read.rs`)

- Supported entities: `LINE`, `ARC`, `CIRCLE`, `LWPOLYLINE` (with bulges),
  `POLYLINE`, `SPLINE`, `ELLIPSE`. Anything else is skipped with a warning.
- Arcs stay analytic. Only splines and ellipses are linearized, at
  `curve_tolerance` (default 0.25 mm chord error). This is the biggest
  quality difference from browser-based nesters, which flatten every
  curve into dense polylines.
- Segments are chained into closed loops by endpoint proximity (0.05 mm,
  then a looser 0.5 mm pass for leftovers). Chains that do not close are
  skipped with a warning. Real CAD exports are often loose `LINE`/`ARC`
  soup with no polylines, so chaining is not optional.
- A containment tree decides roles: depth 0 is an outer contour, depth 1
  its holes, depth 2 another part inside a hole, and so on. Several
  top-level loops in one file become several parts (`name`, `name#2`, …).
- Normalization (in `Part::from_contours`): outers CCW, holes CW, each
  part translated so its bounding box sits at the origin.
- If the header says inches (`$INSUNITS == 1`) everything is scaled by
  25.4 with a warning; otherwise millimeters are assumed.

### 2. Geometry preparation and spacing semantics (`place.rs`, `geom.rs`)

`spacing` (part to part) and `margin` (part to sheet edge) are implemented
by two different mechanisms, which is easy to confuse:

- **spacing**: the NFP is computed between A's raw polygon and B's polygon
  inflated by `spacing`, so any two parts end up at least `spacing` apart.
- **margin**: the sheet rectangle is shrunk by `margin` on every side and
  the inner-fit rectangle is computed against raw polygons.

Rings are simplified at `curve_tolerance` before the NFP math, with the
epsilon doubled until a ring has at most ~200 vertices. Spline-heavy
production parts arrive with thousands of sample points, and the
Minkowski sweep is O(n·m); without this cap a single generation takes
minutes. The guaranteed gap is therefore `spacing` minus roughly twice
the final epsilon (≥ 1.5 mm with the defaults). Output always uses the
exact geometry.

All polygon math goes through Clipper2 on a 0.01 mm integer grid, which is
fine for sheet cutting. `geom.rs` is the only file that touches Clipper2
types; everything else sees `Vec<Pt>`. One consequence of the grid: at
non-orthogonal rotations, quantization can produce tiny "pinhole" hole
rings inside an NFP, and a part placed in one of those would overlap. NFP
holes under 1 mm² are dropped for that reason.

### 3. NFP and candidate positions

The no-fit polygon is the central idea. Given a fixed part A and a part B
to place, NFP(A, B) is the region where B's reference point would make B
overlap A. Conversely, as long as B's reference point lies outside the
union of all NFPs, B overlaps nothing: continuous collision detection
becomes a discrete polygon problem.

The inner-fit polygon (here a rectangle) describes where the reference
point may go for B to lie fully inside the sheet. Candidate positions are
the vertices of the inner-fit rectangle minus every placed part's NFP.
NFPs are cached by (part A, angle A, part B, angle B) and shared across
threads.

### 4. Greedy decoding: order to layout (`place::decode`)

For each gene in order:

1. Try the gene's rotation first; if nothing fits, try the other allowed
   angles in turn.
2. Among the candidate positions pick the lexicographic minimum of
   (strip max-x after placing, x, y): keep the layout from getting
   longer, then go left, then go down. This is gravity towards the left.
3. If the part fits nowhere on the current sheet, open a new one. There
   is no cap on sheets; the objective penalizes them.

### 5. The objective

```
fitness = (sheets_used − 1) · sheet_area + last_max_x · sheet_height
        = sheet_height · [ (sheets_used − 1) · sheet_width + last_max_x ]
```

Factor out the constant `sheet_height` and what remains is the total
length consumed. There is no area term and no height term. Each instance
that cannot be placed anywhere adds a heavy penalty of 2 × sheet area.

> **The trap.** When the sheet width is fixed and the parts overflow onto
> a second sheet, `(sheets_used − 1) · sheet_area` is a constant for the
> sheets that are already full. How well the first sheet is packed has no
> effect on the score at all; the GA can only optimize the tail on the
> last sheet. The landscape is flat, and evolution has nowhere to climb.
> Measured: raising `stale_generations` from 60 to 20000 (1400
> generations) changed the result by not a single digit.

### 6. Auto width

The fix for that trap. With `auto_width` on, the configured width is
replaced by a bound wide enough for every instance in a single row, so
there is always exactly one sheet and the whole layout is back inside the
objective's view.

- The bound is the sum over instances of `max(bbox width, bbox height) +
  spacing`, plus two margins. Deliberately loose: it only caps the search
  space, and `stats.used_width` reports what was really used.
- `NestSolution::sheet_width` is the reached length plus one margin, and
  `render_config` swaps it in, so previews are not a mostly empty strip.
- Under auto width, `utilization` is measured against the reached width.
  Measured against the bound, a good layout would show a meaningless
  19.6%.

### 7. The genetic algorithm (`ga.rs`)

| stage        | what happens                                                                       |
|--------------|------------------------------------------------------------------------------------|
| individual   | a permutation of all instances, each with a rotation index                         |
| initial pop. | individual 0 is the greedy solution (largest area first, rotation 0); rest random   |
| selection    | tournament of two; the best two individuals are carried over unchanged            |
| crossover    | order crossover (OX); the child keeps the rotation gene of the contributing parent |
| mutation     | per gene: swap with the next with `mutation_rate`; independently re-roll rotation  |
| evaluation   | the whole population is decoded in parallel (rayon), sharing the NFP cache         |
| termination  | `time_limit_ms` elapsed, **or** `stale_generations` without improvement, **or** the caller's `should_stop` |

Termination is an OR. If `stale_generations` is too low, the run gives up
long before the time budget is spent; we hit this in production.

Randomness comes from a `StdRng` seeded by `NestConfig::seed`. A seeded
run that terminates on the stale limit is bit-for-bit reproducible. A run
that terminates on the time limit is not, because the generation count
depends on the machine.

### 8. Micro-joints / tabs (`tabs.rs`)

Small gaps left uncut so a part stays attached to the skeleton and cannot
tip up into the laser head. Implemented by splitting each closed contour
into open chains, removing `width` mm of path at each tab position.

- Count = max(`min_per_contour` for outers / 1 for holes,
  ceil(perimeter / `max_spacing`)).
- Ideal positions are spread evenly along the perimeter and offset by half
  a pitch so no tab lands on the start seam.
- Each ideal position snaps to the nearest eligible point: on a straight
  segment, at least `corner_clearance + width/2` from both ends. Arcs are
  allowed only when no straight window is big enough.
- Two tabs may not be closer than 10 × `width` along the perimeter; the
  later one is dropped.
- Holes only get tabs when their bounding box's shorter side is at least
  `min_hole_size` (small slugs just drop through the slats).
- Tabs are applied in local coordinates, so every instance of a part has
  identical tab positions.

### 9. Output (`dxf/write.rs`, `svg.rs`)

- **DXF**: sheets side by side along +X with a 50 mm gap. Layer `SHEET`
  holds the outlines, `CUT` the parts. Arcs are written as true `ARC`
  entities (CCW; CW sweeps are emitted with start/end swapped).
- **SVG**: one `<svg>` per sheet, y flipped (SVG is y-down), parts as
  `<path>` with line and arc commands, `data-part` / `data-instance`
  attributes for UIs. `to_svg_all` stacks the sheets vertically into one
  document.

## Measurements

All runs below use the same batch of real production parts: one job with
5 parts in quantities 5/1/1/5/1 (13 instances), sheet height 910 mm,
spacing 2, margin 5, orthogonal rotation, 20 s budget. Release build,
Apple Silicon, August 2026.

### More generations do nothing at a fixed sheet width

| stale | generations | strip utilization | sheets |
|------:|------------:|------------------:|-------:|
| 60    | 61          | 62.7%             | 2      |
| 300   | 301         | 62.7%             | 2      |
| 2000  | 682 / 1234  | 62.7%             | 2      |
| 20000 | 1427 / 608  | 62.7%             | 2      |

Sheet width fixed at 4000 mm. 61 generations at stale = 60 means only the
first generation ever improved.

### Auto width is the real fix

| mode                 | sheets | width used | utilization |
|----------------------|-------:|-----------:|------------:|
| fixed 4000 mm sheet  | 2      | 6205.6 mm  | 62.7%       |
| auto width           | 1      | 5150.9 mm  | 75.5%       |

Same parts, 1054.7 mm less stock, about 17%.

### Effect of `stale_generations` under auto width

| stale | width used, 3 runs (mm)   | mean | wall time   | generations |
|------:|---------------------------|-----:|-------------|-------------|
| 15    | 5251.8 / 5409.6 / 5719.8  | 5460 | 8.5–12.8 s  | 28–47       |
| 60    | 4965.1 / 5485.6 / 5475.7  | 5309 | 9.9–12.6 s  | 69–121      |
| 150   | 5247.7 / 5419.5 / 5018.9  | 5229 | 10.9–12.0 s | 183–304     |
| 300   | 5018.9 / 5160.8 / 4965.1  | 5048 | 14.4–17.6 s | 441–898     |
| 600   | 4861.8 / 5160.4 / 5176.7  | 5066 | 15.0–20.0 s | 694–1232    |
| 1200  | 5263.8 / 5176.7 / 5389.8  | 5277 | 20.0 s      | 839–1210    |
| 3000  | 5057.8 / 5176.7 / 5384.7  | 5206 | 20.0 s      | 969–1517    |

How to read this: 15–150 is clearly too little, the run quits after 8–13
seconds. From 300 up it is one plateau; the apparent regression at
1200/3000 is noise. What deserves attention is the 500 mm spread between
three runs of the *same* setting: run-to-run randomness matters more than
this knob. The default is 600, which sits on the plateau and spends 15–20
seconds of the budget.

## Known limitations and next steps

**Several restarts beat one long run.** The table above shows randomness
dominating. The same 20 seconds split into three independent 6-second
runs, keeping the best, has a better expected result than one 20-second
run. This is the most valuable optimization not yet implemented.

**Small parts do not go inside large parts' holes.** The decoder only
considers the sheet's inner-fit rectangle.

**Free rotation is slow on curvy parts.** 24 angles × NFP per pair; the
first generation can eat the whole budget on spline-heavy inputs.

**Parameters are product decisions, not tuning constants.** Raising
`stale_generations` trades latency for layout quality. Do not lower it to
make jobs feel faster. And remember: when the sheet width is pinned and
the layout overflows, this knob does nothing at all; the fix there is
`auto_width`.

**Read the right metric.**

- Use `used_width`: the length actually consumed, which is what the
  fitness minimizes.
- Use `strip_utilization`: net area over the area actually spanned.
- Be careful with `utilization`: area over (sheets × full sheet area).
  Meaningful only when the sheet width is a real material limit; under
  auto width it is defined to equal the strip version.
