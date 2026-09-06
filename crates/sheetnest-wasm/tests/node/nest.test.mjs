// Run with: node --test tests/node/
//
// Resolves the package by name through `node_modules/sheetnest` -> `pkg`,
// the symlink scripts/build.sh creates, so the real `exports` map is what is
// under test and not a path into the build directory.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { Part, nest, version } from "sheetnest";

const require = createRequire(import.meta.url);
const fixture = (name) =>
  readFileSync(fileURLToPath(new URL(`../../../../fixtures/${name}.dxf`, import.meta.url)));

const FIXTURES = ["bracket_l", "disc", "gusset", "plate_rounded", "strip"];

const rect = (name, w, h, qty = 1) =>
  Part.fromPolygon(name, qty, [
    [0, 0],
    [w, 0],
    [w, h],
    [0, h],
  ]);

// ---------------------------------------------------------------------------

test("module loads the same way from ESM and CJS", () => {
  const cjs = require("sheetnest");
  assert.equal(typeof version(), "string");
  assert.match(version(), /^\d+\.\d+\.\d+/);
  assert.equal(cjs.version(), version());
  // `import { … }` above and `require()` here must reach the same build.
  assert.equal(cjs.nest, nest);
  assert.equal(cjs.Part, Part);
});

test("Part.fromPolygon exposes geometry", () => {
  const p = Part.fromPolygon(
    "plate",
    3,
    [
      [0, 0],
      [120, 0],
      [120, 80],
      [0, 80],
    ],
    [
      [
        [10, 10],
        [20, 10],
        [20, 20],
        [10, 20],
      ],
    ],
  );
  assert.equal(p.name, "plate");
  assert.equal(p.quantity, 3);
  p.quantity = 4;
  assert.equal(p.quantity, 4);
  assert.equal(p.grossArea, 120 * 80);
  assert.equal(p.netArea, 120 * 80 - 100);
  assert.deepEqual(p.bbox, { width: 120, height: 80 });
});

test("nest places four rectangles and is deterministic under a seed", () => {
  const cfg = {
    sheetWidth: 500,
    sheetHeight: 300,
    seed: 1,
    staleGenerations: 30,
    timeLimitMs: 20000,
  };
  const parts = () => [
    rect("a", 200, 120),
    rect("b", 180, 140),
    rect("c", 150, 100),
    rect("d", 220, 90),
  ];

  const first = nest(parts(), cfg);
  assert.equal(first.stats.placed, first.stats.total);
  assert.equal(first.stats.total, 4);
  assert.equal(first.stats.stopReason, "stale");
  assert.equal(first.sheetWidth, 500);
  assert.equal(first.sheetHeight, 300);
  assert.equal(first.placements.length, 4);

  const second = nest(parts(), cfg);
  assert.deepEqual(second.toJSON().placements, first.toJSON().placements);
  assert.equal(second.stats.utilization, first.stats.utilization);
});

test("Part handles stay usable after nest", () => {
  const parts = [rect("keep", 100, 50, 2)];
  const solution = nest(parts, { sheetWidth: 500, sheetHeight: 300, seed: 1, staleGenerations: 20 });
  assert.equal(solution.stats.placed, 2);
  // `nest` copies the geometry rather than taking the handles over.
  assert.equal(parts[0].name, "keep");
  assert.equal(parts[0].quantity, 2);
});

test("nests the DXF fixtures and renders DXF and SVG back out", () => {
  const parts = [];
  for (const name of FIXTURES) {
    const parsed = Part.fromDxf(fixture(name), `${name}.dxf`, 2);
    assert.ok(Array.isArray(parsed.warnings));
    assert.ok(parsed.parts.length >= 1, `${name} produced no parts`);
    parts.push(...parsed.parts);
  }

  const started = Date.now();
  const solution = nest(parts, {
    autoWidth: true,
    sheetHeight: 914,
    spacing: 2,
    margin: 5,
    seed: 7,
    staleGenerations: 40,
    timeLimitMs: 30000,
    tabs: { enabled: true },
  });
  const elapsed = Date.now() - started;

  assert.equal(solution.stats.placed, solution.stats.total);
  assert.equal(solution.stats.sheetsUsed, 1);
  assert.ok(solution.stats.utilization > 0 && solution.stats.utilization <= 1);
  assert.equal(typeof solution.stats.elapsedMs, "number");

  const dxf = solution.toDxf();
  assert.ok(dxf instanceof Uint8Array);
  assert.ok(dxf.byteLength > 1000, `dxf was only ${dxf.byteLength} bytes`);
  assert.match(Buffer.from(dxf).toString("latin1"), /SECTION/);

  assert.match(solution.toSvg(), /<svg/);
  assert.match(solution.toSvgAll(), /<svg/);

  const json = solution.toJSON();
  assert.equal(JSON.parse(JSON.stringify(json)).stats.placed, solution.stats.placed);

  console.log(
    `    fixtures: ${parts.length} parts / ${solution.stats.total} instances ` +
      `in ${elapsed}ms (${solution.stats.generations} generations), ` +
      `utilization ${(solution.stats.utilization * 100).toFixed(1)}%`,
  );
});

test("onProgress receives numeric snapshots", () => {
  const seen = [];
  nest([rect("p", 100, 60, 6)], { sheetWidth: 500, sheetHeight: 300, seed: 3, staleGenerations: 10 }, (p) =>
    seen.push(p),
  );
  assert.ok(seen.length >= 1, "progress callback was never called");
  for (const p of seen) {
    for (const key of ["generation", "bestFitness", "bestUtilization", "elapsedMs"]) {
      assert.equal(typeof p[key], "number", `${key} was ${typeof p[key]}`);
      assert.ok(Number.isFinite(p[key]));
    }
  }
});

test("returning true from onProgress cancels the run", () => {
  let calls = 0;
  const solution = nest(
    [rect("p", 100, 60, 8)],
    { sheetWidth: 500, sheetHeight: 300, seed: 3, staleGenerations: 5000, timeLimitMs: 60000 },
    () => ++calls >= 2,
  );
  assert.equal(solution.stats.stopReason, "cancelled");
  assert.ok(calls <= 3, `callback ran ${calls} times after asking to stop`);
});

test("an exception from onProgress propagates out of nest", () => {
  assert.throws(
    () =>
      nest([rect("p", 100, 60, 4)], { sheetWidth: 500, sheetHeight: 300, seed: 3 }, () => {
        throw new RangeError("boom");
      }),
    /boom/,
  );
});

test("an unknown config field is an error", () => {
  assert.throws(
    () => nest([rect("p", 10, 10)], { sheetwidth: 500 }),
    (e) => e instanceof TypeError && /unknown config field "sheetwidth"/.test(e.message),
  );
  assert.throws(
    () => nest([rect("p", 10, 10)], { tabs: { enabled: true, wdith: 2 } }),
    /unknown config field tabs."wdith"/,
  );
  assert.throws(() => nest([rect("p", 10, 10)], { rotationMode: "sideways" }), /invalid config/);
});

test("a config is optional and partial", () => {
  const solution = nest([rect("p", 100, 60, 2)]);
  assert.equal(solution.stats.placed, 2);
  assert.equal(solution.sheetWidth, 1829);
  assert.equal(solution.sheetHeight, 914);
});

test("degenerate input is rejected", () => {
  assert.throws(
    () =>
      Part.fromPolygon("line", 1, [
        [0, 0],
        [5, 0],
      ]),
    /degenerate|3\+ points/,
  );
  assert.throws(() => Part.fromPolygon("nope", 1, "not an array"), TypeError);
  assert.throws(() => Part.fromDxf(new Uint8Array([1, 2, 3]), "junk.dxf"), /DXF/);
  assert.throws(() => nest([{}]), /sheetnest Part/);
});

test("a freed Part is rejected rather than read", () => {
  const p = rect("gone", 10, 10);
  p.free();
  assert.throws(() => nest([p]), TypeError);
});
