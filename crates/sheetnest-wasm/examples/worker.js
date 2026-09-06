// Nesting blocks for up to `timeLimitMs`, so it runs here and not on the main
// thread. Served from the same directory as pkg/web/ (see index.html).
import init, { Part, nest } from "../pkg/web/sheetnest.js";

let cancelled = false;

self.onmessage = async ({ data }) => {
  if (data.type === "cancel") return void (cancelled = true);

  await init(); // fetch + instantiate the .wasm (cached after the first call)
  cancelled = false;

  try {
    const { parts, warnings } = Part.fromDxf(new Uint8Array(data.dxf), data.name, data.quantity);
    const solution = nest(parts, data.config, (p) => {
      self.postMessage({ type: "progress", ...p });
      return cancelled; // a truthy return stops the run
    });
    const dxf = solution.toDxf();
    self.postMessage(
      { type: "done", stats: solution.stats, svg: solution.toSvgAll(), dxf, warnings },
      [dxf.buffer],
    );
  } catch (e) {
    self.postMessage({ type: "error", message: String(e?.message ?? e) });
  }
};
