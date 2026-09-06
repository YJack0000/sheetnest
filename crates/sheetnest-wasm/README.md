# sheetnest

2D nesting for sheet cutting: pack parts onto rectangular stock with as little
waste as possible. WebAssembly build of the Rust
[sheetnest](https://github.com/YJack0000/sheetnest) engine — a no-fit-polygon
placer driven by a genetic algorithm, the same family as SVGnest, running on
Clipper2.

- Reads and writes **DXF**, and arcs stay analytic all the way through, so a
  laser or plasma cutter gets true `ARC` entities rather than dense polylines.
- Optional **micro-joints (tabs)** keep cut parts from tipping into the head.
- Renders sheets as **SVG** for previews.
- No native addon, no build step: one `.wasm` that runs in browsers, Node,
  Deno, Bun and edge runtimes.

All lengths are millimeters, all angles degrees, y is up.

```sh
npm install sheetnest
```

## Node

```js
import { readFileSync } from "node:fs";
import { Part, nest } from "sheetnest";

const { parts, warnings } = Part.fromDxf(readFileSync("bracket.dxf"), "bracket.dxf", 12);
for (const w of warnings) console.warn(w);

const solution = nest(parts, {
  sheetWidth: 1829,
  sheetHeight: 914,
  spacing: 2,
  margin: 5,
  timeLimitMs: 10_000,
  tabs: { enabled: true },
});

console.log(`${solution.stats.placed}/${solution.stats.total} placed on ` +
  `${solution.stats.sheetsUsed} sheet(s), ` +
  `${(solution.stats.utilization * 100).toFixed(1)}% utilization`);

writeFileSync("nested.dxf", solution.toDxf());
writeFileSync("nested.svg", solution.toSvgAll());
```

`require("sheetnest")` works too — the Node build is CommonJS, the browser
build is ESM, and the `exports` map picks the right one.

## Browser

**A nest blocks for up to `timeLimitMs`.** There is no yielding inside the
optimizer, so running it on the main thread freezes the page. Run it in a Web
Worker and post progress back — that is also how you get a live progress bar
and a working cancel button.

`examples/worker.js`:

```js
import init, { Part, nest } from "sheetnest";

let cancelled = false;

self.onmessage = async ({ data }) => {
  if (data.type === "cancel") return void (cancelled = true);

  await init();                      // fetch + instantiate the .wasm
  cancelled = false;

  const { parts, warnings } = Part.fromDxf(new Uint8Array(data.dxf), data.name, data.quantity);
  const solution = nest(parts, data.config, (p) => {
    self.postMessage({ type: "progress", ...p });
    return cancelled;                // truthy return stops the run
  });

  self.postMessage({
    type: "done",
    stats: solution.stats,
    svg: solution.toSvgAll(),
    dxf: solution.toDxf(),           // Uint8Array; transferable
    warnings,
  });
};
```

`examples/index.html` drives it: pick a DXF, watch the generations tick by,
cancel at any point, then download the nested DXF. (The shipped example
imports `../pkg/web/sheetnest.js` rather than the bare package name, so it
runs straight off a static file server with no bundler.)

The `default` export condition is the ESM/browser build, so bundlers pick it
up automatically. Without a bundler, import
`sheetnest/web/sheetnest.js` directly and make sure `sheetnest_bg.wasm` is
served next to it.

## API

### `Part`

| Member | Meaning |
| --- | --- |
| `Part.fromPolygon(name, quantity, outer, holes?)` | Build from vertex rings. `outer` and each hole is an array of `[x, y]` pairs (or `{x, y}` objects), either winding order, closing vertex optional. Throws on a degenerate ring. |
| `Part.fromDxf(bytes, name, quantity?, curveTolerance?)` | Parse a DXF. Returns `{ parts: Part[], warnings: string[] }`. `quantity` defaults to `1`, `curveTolerance` (max chord error in mm when linearizing splines) to `0.25`. |
| `part.name` | Part name, from the DXF layer or the name you passed. |
| `part.quantity` | How many to place. Writable. |
| `part.grossArea` / `part.netArea` | Outer area, and outer minus holes, mm². |
| `part.bbox` | `{ width, height }` of the unrotated part, mm. |
| `part.free()` | Release the geometry. Optional — the engine's finalizer does it — but explicit is cheaper. |

`nest` copies the geometry it needs, so parts stay usable afterwards and can
be nested more than once.

### `nest(parts, config?, onProgress?) => Solution`

`onProgress(p)` is called once per generation with
`{ generation, bestFitness, bestUtilization, elapsedMs }`. **Return a truthy
value to cancel**; the solution then reports `stats.stopReason === "cancelled"`.
An exception thrown from the callback stops the run and propagates out of
`nest`.

### `Solution`

| Member | Meaning |
| --- | --- |
| `solution.placements` | `{ partId, partName, instance, sheet, rotationDeg, dx, dy }[]`. `partId` indexes the array you passed to `nest`. Rotate the part's local geometry by `rotationDeg` counter-clockwise about the origin, then translate by `(dx, dy)`. |
| `solution.stats` | See below. |
| `solution.sheetWidth` / `sheetHeight` | The sheet the placements are laid out on. Under `autoWidth` the width is the length the layout actually reached. |
| `solution.warnings` | Parts that could not be placed, and why. |
| `solution.toJSON()` | The whole thing as a plain object; `JSON.stringify` works on it. |
| `solution.toDxf()` | `Uint8Array` DXF: every sheet side by side along +X, sheet outlines on layer `SHEET`, cut geometry on `CUT`. |
| `solution.toSvg(sheet?)` | One sheet as an SVG document (default `0`). |
| `solution.toSvgAll()` | Every sheet stacked vertically in one SVG. |

`stats` is `{ stopReason, sheetsUsed, usedWidth, utilization, stripUtilization,
generations, elapsedMs, placed, total }`. `stopReason` is `"time_limit"`,
`"stale"`, `"cancelled"` or `"empty"`. `usedWidth` — the stock actually
consumed along X — is what the optimizer minimizes and the honest headline
number; `utilization` is an area ratio that moves for reasons the optimizer
does not care about.

### `version()`

The package version, e.g. `"0.1.0"`.

## Config

Every field is optional. An unrecognized field name is a `TypeError` rather
than a silent default.

| Field | Default | Meaning |
| --- | --- | --- |
| `sheetWidth` | `1829` | Usable stock length along X, mm. Ignored when `autoWidth` is set. |
| `sheetHeight` | `914` | Usable stock height along Y, mm. |
| `autoWidth` | `false` | Treat the stock as a strip of unbounded length. A second sheet is never opened, and `stats.usedWidth` is how far along the coil to cut. |
| `spacing` | `2` | Minimum gap between parts, mm. |
| `margin` | `5` | Minimum gap between parts and the sheet edge, mm. |
| `rotationMode` | `"orthogonal"` | `"orthogonal"` allows 0/90/180/270; `"free"` allows multiples of `rotationStepDeg`. |
| `rotationStepDeg` | `15` | Rotation granularity under `"free"`, degrees. |
| `curveTolerance` | `0.25` | Max chord error when linearizing curves for the nesting math, mm. |
| `timeLimitMs` | `20000` | Hard stop for the search. |
| `population` | `15` | GA population size. |
| `mutationRate` | `0.10` | 0.0 – 1.0. |
| `staleGenerations` | `600` | Give up early after this many generations without improvement. Lower is faster and worse; this is a product call, not a tuning constant. |
| `seed` | *(random)* | Seed the GA. With a seed, the same input gives the same layout **whenever the run ends on `staleGenerations`** — a wall-clock stop cuts the search at a machine-dependent generation, so pair `seed` with a `staleGenerations` the run can actually reach. |
| `tabs.enabled` | `false` | Leave micro-joints so cut parts do not tip into the head. |
| `tabs.width` | `0.3` | Gap width left uncut, mm. |
| `tabs.maxSpacing` | `250` | Max perimeter distance between adjacent tabs, mm. |
| `tabs.minPerContour` | `2` | Minimum tabs on each outer contour. |
| `tabs.cornerClearance` | `3` | Keep tabs at least this far from corners, mm. |
| `tabs.minHoleSize` | `40` | Holes whose bbox min-dimension is below this get no tabs, mm. |

## Limitations

- **Single-threaded.** The native crate evaluates the GA population with rayon;
  the wasm build cannot, so expect it to be several times slower than the same
  job on a desktop core. Budget `timeLimitMs` accordingly.
- **Blocking.** `nest` does not yield. In a browser it belongs in a Worker; in
  a request handler it will hold the thread for `timeLimitMs`.
- **Size.** ~1.7 MB of wasm, ~580 KB gzipped, ~440 KB brotli. Serve it
  compressed and let it cache.
- **Memory.** 32-bit: the module cannot address more than 4 GB, and a job with
  thousands of instances at a fine `curveTolerance` will feel it long before
  that. The no-fit-polygon cache is the bulk of it.
- **Determinism** holds for a given build and `seed` only when the run ends on
  `staleGenerations`; see the config table.

## License

MIT OR Apache-2.0, at your option.
