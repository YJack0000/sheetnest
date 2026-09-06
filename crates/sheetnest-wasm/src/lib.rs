//! WebAssembly bindings for [`sheetnest`](https://crates.io/crates/sheetnest),
//! published to npm as the `sheetnest` package.
//!
//! The whole crate is `wasm32`-only: it is a `cdylib` of JS glue and there is
//! nothing to build for a host target. Compiling to nothing elsewhere keeps
//! `cargo build --workspace` and `cargo fmt --all` working on a dev machine
//! without a `--exclude`.
#![cfg(target_arch = "wasm32")]

mod cxx_abi;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sheetnest::model::ring_bbox;
use sheetnest::{Hooks, NestConfig, NestSolution, Pt};
use wasm_bindgen::prelude::*;

/// Version of the npm package, e.g. `"0.1.0"`.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Route Rust panics to `console.error` with a real stack trace instead of
/// the bare `unreachable` trap. Runs automatically on module init.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

fn js_err(e: impl Display) -> JsValue {
    js_sys::Error::new(&e.to_string()).into()
}

fn type_err(msg: &str) -> JsValue {
    js_sys::TypeError::new(msg).into()
}

// ---------------------------------------------------------------------------
// Send shim
// ---------------------------------------------------------------------------

/// Makes a non-`Send` value (a `js_sys::Function`, a `RefCell`) satisfy the
/// `Send + Sync` bounds on [`Hooks`].
///
/// Sound here because this crate only ever compiles for
/// `wasm32-unknown-unknown` (see the crate-level `cfg`), which is built
/// without threads or atomics: there is exactly one thread, the hooks are
/// called synchronously from `nest` on that thread, and the core's `parallel`
/// (rayon) feature is off for this build. Nothing can observe the value from
/// a second thread because no second thread exists.
struct SendCell<T>(T);

// SAFETY: single-threaded target, see the type docs.
unsafe impl<T> Send for SendCell<T> {}
// SAFETY: single-threaded target, see the type docs.
unsafe impl<T> Sync for SendCell<T> {}

// ---------------------------------------------------------------------------
// Part
// ---------------------------------------------------------------------------

thread_local! {
    /// Parts owned by this module, keyed by the id their JS handle carries.
    ///
    /// wasm-bindgen can hand a `Part` to JS and take one back by value, but
    /// there is no public way to *borrow* the Rust struct behind an element of
    /// a `js_sys::Array`: the generated conversion moves the object out and
    /// nulls the caller's handle. So the handle carries an id instead of the
    /// geometry, `nest` looks the id up here, and the caller's `Part` objects
    /// stay usable after the call.
    static PARTS: RefCell<HashMap<u32, sheetnest::Part>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };

    /// Identity token proving a JS object is a `Part` minted by *this* module
    /// instance. Compared by reference, so a `Part` from a second copy of the
    /// module loaded into the same realm is rejected instead of being looked
    /// up against our unrelated registry.
    static PART_BRAND: JsValue = js_sys::Object::new().into();
}

/// One part to nest: an outer contour plus zero or more holes, in
/// millimeters. Build one with `Part.fromPolygon` or `Part.fromDxf`.
///
/// Call `free()` when done with it, or let the engine's finalizer do it.
// The handle carries only a registry key; see `PARTS`.
#[wasm_bindgen]
pub struct Part {
    id: u32,
}

impl Drop for Part {
    fn drop(&mut self) {
        PARTS.with(|m| m.borrow_mut().remove(&self.id));
    }
}

impl Part {
    fn register(part: sheetnest::Part) -> Part {
        let id = NEXT_ID.with(|n| {
            let id = n.get();
            n.set(id.wrapping_add(1).max(1));
            id
        });
        PARTS.with(|m| m.borrow_mut().insert(id, part));
        Part { id }
    }

    /// Run `f` on the geometry behind this handle. The entry exists for as
    /// long as the handle does, so a miss means memory corruption, not a
    /// caller error.
    fn with<R>(&self, f: impl FnOnce(&sheetnest::Part) -> R) -> R {
        PARTS.with(|m| {
            let m = m.borrow();
            f(m.get(&self.id)
                .expect("live Part handle without registry entry"))
        })
    }
}

#[wasm_bindgen]
impl Part {
    /// Build a part from plain vertex rings. `outer` and each entry of
    /// `holes` is an array of `[x, y]` pairs (or `{x, y}` objects); either
    /// winding order works, and a repeated closing vertex is optional.
    #[wasm_bindgen(js_name = fromPolygon)]
    pub fn from_polygon(
        name: &str,
        quantity: u32,
        outer: &JsValue,
        holes: Option<js_sys::Array>,
    ) -> Result<Part, JsValue> {
        let outer = read_ring(outer, "outer")?;
        let mut hole_rings = Vec::new();
        if let Some(hs) = holes {
            for (i, h) in hs.iter().enumerate() {
                hole_rings.push(read_ring(&h, &format!("holes[{i}]"))?);
            }
        }
        let part =
            sheetnest::Part::from_polygon(name, quantity, &outer, &hole_rings).map_err(js_err)?;
        Ok(Part::register(part))
    }

    /// Parse a DXF drawing into parts.
    ///
    /// Returns `{ parts: Part[], warnings: string[] }`. `quantity` defaults
    /// to 1, `curveTolerance` (max chord error in mm when linearizing
    /// splines) to 0.25.
    #[wasm_bindgen(js_name = fromDxf)]
    pub fn from_dxf(
        bytes: &[u8],
        name: &str,
        quantity: Option<u32>,
        curve_tolerance: Option<f64>,
    ) -> Result<JsValue, JsValue> {
        let parsed = sheetnest::dxf::parse_dxf(
            bytes,
            name,
            quantity.unwrap_or(1),
            curve_tolerance.unwrap_or(0.25),
        )
        .map_err(js_err)?;

        let parts = js_sys::Array::new();
        for p in parsed.parts {
            parts.push(&JsValue::from(Part::register(p)));
        }
        let warnings = js_sys::Array::new();
        for w in &parsed.warnings {
            warnings.push(&JsValue::from_str(w));
        }

        let out = js_sys::Object::new();
        js_sys::Reflect::set(&out, &JsValue::from_str("parts"), &parts)?;
        js_sys::Reflect::set(&out, &JsValue::from_str("warnings"), &warnings)?;
        Ok(out.into())
    }

    #[wasm_bindgen(getter)]
    pub fn name(&self) -> String {
        self.with(|p| p.name.clone())
    }

    #[wasm_bindgen(getter)]
    pub fn quantity(&self) -> u32 {
        self.with(|p| p.quantity)
    }

    #[wasm_bindgen(setter)]
    pub fn set_quantity(&mut self, quantity: u32) {
        PARTS.with(|m| {
            if let Some(p) = m.borrow_mut().get_mut(&self.id) {
                p.quantity = quantity;
            }
        });
    }

    /// Area enclosed by the outer contour, mm².
    #[wasm_bindgen(getter, js_name = grossArea)]
    pub fn gross_area(&self) -> f64 {
        self.with(|p| p.gross_area)
    }

    /// Outer area minus the holes, mm².
    #[wasm_bindgen(getter, js_name = netArea)]
    pub fn net_area(&self) -> f64 {
        self.with(|p| p.net_area)
    }

    /// Bounding box of the unrotated part as `{ width, height }`, mm.
    #[wasm_bindgen(getter)]
    pub fn bbox(&self) -> Result<JsValue, JsValue> {
        let (minx, miny, maxx, maxy) = self.with(|p| ring_bbox(&p.outer_poly));
        let out = js_sys::Object::new();
        js_sys::Reflect::set(
            &out,
            &JsValue::from_str("width"),
            &JsValue::from_f64(maxx - minx),
        )?;
        js_sys::Reflect::set(
            &out,
            &JsValue::from_str("height"),
            &JsValue::from_f64(maxy - miny),
        )?;
        Ok(out.into())
    }

    /// Internal. Not public API.
    #[wasm_bindgen(getter, js_name = __sheetnestId)]
    pub fn sheetnest_id(&self) -> u32 {
        self.id
    }

    /// Internal. Not public API.
    #[wasm_bindgen(getter, js_name = __sheetnestBrand)]
    pub fn sheetnest_brand(&self) -> JsValue {
        PART_BRAND.with(JsValue::clone)
    }
}

/// Read one `[x, y][]` (or `{x, y}[]`) ring out of JS.
fn read_ring(value: &JsValue, what: &str) -> Result<Vec<Pt>, JsValue> {
    let arr: &js_sys::Array = value
        .dyn_ref::<js_sys::Array>()
        .ok_or_else(|| type_err(&format!("{what} must be an array of [x, y] points")))?;
    let mut ring = Vec::with_capacity(arr.length() as usize);
    for (i, p) in arr.iter().enumerate() {
        let (x, y) = if let Some(pair) = p.dyn_ref::<js_sys::Array>() {
            (pair.get(0).as_f64(), pair.get(1).as_f64())
        } else if p.is_object() {
            (
                js_sys::Reflect::get(&p, &JsValue::from_str("x"))?.as_f64(),
                js_sys::Reflect::get(&p, &JsValue::from_str("y"))?.as_f64(),
            )
        } else {
            (None, None)
        };
        match (x, y) {
            (Some(x), Some(y)) => ring.push(Pt::new(x, y)),
            _ => {
                return Err(type_err(&format!(
                    "{what}[{i}] must be [x, y] or {{ x, y }} with finite numbers"
                )));
            }
        }
    }
    Ok(ring)
}

/// Copy the geometry behind a JS `Part` handle, leaving the handle usable.
fn clone_part_from_js(value: &JsValue) -> Result<sheetnest::Part, JsValue> {
    let stale = || type_err("parts must be live sheetnest Part instances (already freed?)");
    let brand =
        js_sys::Reflect::get(value, &JsValue::from_str("__sheetnestBrand")).map_err(|_| stale())?;
    if !PART_BRAND.with(|b| brand.eq(b)) {
        return Err(type_err("parts must be sheetnest Part instances"));
    }
    let id = js_sys::Reflect::get(value, &JsValue::from_str("__sheetnestId"))
        .map_err(|_| stale())?
        .as_f64()
        .ok_or_else(stale)? as u32;
    PARTS
        .with(|m| m.borrow().get(&id).cloned())
        .ok_or_else(stale)
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

const CONFIG_KEYS: &[&str] = &[
    "sheetWidth",
    "autoWidth",
    "sheetHeight",
    "spacing",
    "margin",
    "rotationMode",
    "rotationStepDeg",
    "curveTolerance",
    "timeLimitMs",
    "population",
    "mutationRate",
    "staleGenerations",
    "seed",
    "tabs",
];

const TAB_KEYS: &[&str] = &[
    "enabled",
    "width",
    "maxSpacing",
    "minPerContour",
    "cornerClearance",
    "minHoleSize",
];

/// Reject unknown keys before handing the object to serde.
///
/// `NestConfig` is `#[serde(default)]` without `deny_unknown_fields`, so a
/// typo like `sheetwidth` would otherwise be silently ignored and the run
/// would quietly use the default sheet. Checking here keeps the core's
/// forgiving JSON behaviour while making the JS API strict.
fn reject_unknown_keys(obj: &JsValue, known: &[&str], path: &str) -> Result<(), JsValue> {
    let keys = js_sys::Object::keys(obj.unchecked_ref::<js_sys::Object>());
    for k in keys.iter() {
        let k = k.as_string().unwrap_or_default();
        if !known.contains(&k.as_str()) {
            return Err(type_err(&format!(
                "unknown config field {path}{k:?}; expected one of: {}",
                known.join(", ")
            )));
        }
    }
    Ok(())
}

fn parse_config(value: &JsValue) -> Result<NestConfig, JsValue> {
    if value.is_undefined() || value.is_null() {
        return Ok(NestConfig::default());
    }
    if !value.is_object() {
        return Err(type_err("config must be an object"));
    }
    reject_unknown_keys(value, CONFIG_KEYS, "")?;
    let tabs = js_sys::Reflect::get(value, &JsValue::from_str("tabs"))?;
    if tabs.is_object() {
        reject_unknown_keys(&tabs, TAB_KEYS, "tabs.")?;
    }
    serde_wasm_bindgen::from_value::<NestConfig>(value.clone())
        .map_err(|e| type_err(&format!("invalid config: {e}")))
}

// ---------------------------------------------------------------------------
// nest
// ---------------------------------------------------------------------------

/// Nest `parts` onto sheets described by `config`.
///
/// `config` is a plain object with camelCase fields (see the README table);
/// anything missing takes its default, and an unrecognized field is an
/// error. `onProgress` is called once per generation with
/// `{ generation, bestFitness, bestUtilization, elapsedMs }`; returning a
/// truthy value from it cancels the run, which then reports
/// `stats.stopReason === "cancelled"`.
///
/// This blocks for up to `timeLimitMs`, so in a browser call it from a Web
/// Worker.
#[wasm_bindgen]
pub fn nest(
    parts: &js_sys::Array,
    config: Option<js_sys::Object>,
    on_progress: Option<js_sys::Function>,
) -> Result<Solution, JsValue> {
    let owned: Vec<sheetnest::Part> = parts
        .iter()
        .map(|p| clone_part_from_js(&p))
        .collect::<Result<_, _>>()?;

    let cfg = parse_config(&config.map(JsValue::from).unwrap_or(JsValue::UNDEFINED))?;

    let cancel = Arc::new(AtomicBool::new(false));
    let thrown: Arc<SendCell<RefCell<Option<JsValue>>>> = Arc::new(SendCell(RefCell::new(None)));

    let mut hooks = Hooks::new();
    if let Some(f) = on_progress {
        let cb = SendCell(f);
        let flag = cancel.clone();
        let slot = thrown.clone();
        hooks = hooks
            .on_progress(move |p| {
                let arg = serde_wasm_bindgen::to_value(p).unwrap_or(JsValue::UNDEFINED);
                match cb.0.call1(&JsValue::NULL, &arg) {
                    Ok(ret) => {
                        if ret.is_truthy() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        // The callback threw: stop the run and re-raise it
                        // from `nest` rather than swallowing it.
                        *slot.0.borrow_mut() = Some(e);
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            })
            .should_stop({
                let flag = cancel.clone();
                move || flag.load(Ordering::Relaxed)
            });
    }

    let sol = sheetnest::nest(&owned, &cfg, hooks).map_err(js_err);

    if let Some(e) = thrown.0.borrow_mut().take() {
        return Err(e);
    }

    Ok(Solution {
        sol: sol?,
        parts: owned,
        cfg,
    })
}

// ---------------------------------------------------------------------------
// Solution
// ---------------------------------------------------------------------------

/// The result of a `nest` run, together with the parts and config needed to
/// render it back out as DXF or SVG.
#[wasm_bindgen]
pub struct Solution {
    sol: NestSolution,
    parts: Vec<sheetnest::Part>,
    cfg: NestConfig,
}

#[wasm_bindgen]
impl Solution {
    /// The whole solution as a plain object:
    /// `{ placements, stats, sheetWidth, sheetHeight, warnings }`.
    #[wasm_bindgen(js_name = toJSON)]
    pub fn to_json(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&self.sol).map_err(js_err)
    }

    /// `{ partId, partName, instance, sheet, rotationDeg, dx, dy }[]`.
    #[wasm_bindgen(getter)]
    pub fn placements(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&self.sol.placements).map_err(js_err)
    }

    /// `{ stopReason, sheetsUsed, usedWidth, utilization, stripUtilization,
    /// generations, elapsedMs, placed, total }`.
    #[wasm_bindgen(getter)]
    pub fn stats(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&self.sol.stats).map_err(js_err)
    }

    /// Width the placements are laid out on, mm. Under `autoWidth` this is
    /// the length the layout actually reached, not the configured sheet.
    #[wasm_bindgen(getter, js_name = sheetWidth)]
    pub fn sheet_width(&self) -> f64 {
        self.sol.sheet_width
    }

    #[wasm_bindgen(getter, js_name = sheetHeight)]
    pub fn sheet_height(&self) -> f64 {
        self.sol.sheet_height
    }

    #[wasm_bindgen(getter)]
    pub fn warnings(&self) -> Vec<String> {
        self.sol.warnings.clone()
    }

    /// The nested layout as a DXF file: every sheet side by side along +X,
    /// sheet outlines on layer `SHEET`, cut geometry on `CUT`.
    #[wasm_bindgen(js_name = toDxf)]
    pub fn to_dxf(&self) -> Result<Vec<u8>, JsValue> {
        sheetnest::to_dxf(&self.sol, &self.parts, &self.cfg).map_err(js_err)
    }

    /// One sheet rendered as an SVG document (default sheet 0).
    #[wasm_bindgen(js_name = toSvg)]
    pub fn to_svg(&self, sheet: Option<usize>) -> String {
        sheetnest::to_svg(&self.sol, &self.parts, &self.cfg, sheet.unwrap_or(0))
    }

    /// Every sheet stacked vertically in one SVG document.
    #[wasm_bindgen(js_name = toSvgAll)]
    pub fn to_svg_all(&self) -> String {
        sheetnest::to_svg_all(&self.sol, &self.parts, &self.cfg)
    }
}
