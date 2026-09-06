//! Python bindings for [`sheetnest`](https://docs.rs/sheetnest).
//!
//! The module is built with maturin as `sheetnest._sheetnest` and re-exported
//! from the `sheetnest` package. Everything here is a thin, Pythonic wrapper
//! over the core crate: names are snake_case, lengths are millimeters, angles
//! degrees, y is up.
//!
//! The GA runs with the GIL released ([`Python::detach`]); the progress and
//! cancellation hooks re-attach to call back into Python, and
//! `Python::check_signals` is polled once per generation so Ctrl-C works.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use serde_json::Value;

use sheetnest::model::ring_bbox;
use sheetnest::{
    Hooks, NestConfig, NestSolution, Part, Placement, Progress, Pt, RotationMode, StopReason,
    TabConfig,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn value_err(e: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(e.to_string())
}

fn runtime_err(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

fn rotation_mode_str(m: RotationMode) -> &'static str {
    match m {
        RotationMode::Orthogonal => "orthogonal",
        RotationMode::Free => "free",
    }
}

fn rotation_mode_from_str(s: &str) -> PyResult<RotationMode> {
    match s.to_ascii_lowercase().as_str() {
        "orthogonal" => Ok(RotationMode::Orthogonal),
        "free" => Ok(RotationMode::Free),
        other => Err(PyValueError::new_err(format!(
            "rotation_mode must be 'orthogonal' or 'free', got {other:?}"
        ))),
    }
}

fn stop_reason_str(r: StopReason) -> &'static str {
    match r {
        StopReason::TimeLimit => "time_limit",
        StopReason::Stale => "stale",
        StopReason::Cancelled => "cancelled",
        StopReason::Empty => "empty",
    }
}

fn to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut up = false;
    for c in s.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Rewrite every object key in `v` through `f`, recursively.
fn map_keys(v: Value, f: &dyn Fn(&str) -> String) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (f(&k), map_keys(v, f)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| map_keys(v, f)).collect()),
        other => other,
    }
}

/// Serialize `v`, rename the camelCase keys serde emits to snake_case, and
/// hand the result to Python as plain dicts/lists/scalars.
fn serde_to_py<'py, T: serde::Serialize>(py: Python<'py>, v: &T) -> PyResult<Bound<'py, PyAny>> {
    let json = serde_json::to_value(v).map_err(runtime_err)?;
    pythonize::pythonize(py, &map_keys(json, &to_snake)).map_err(runtime_err)
}

/// Turn a Python mapping into a [`NestConfig`], accepting either camelCase or
/// snake_case keys.
fn config_from_mapping(obj: &Bound<'_, PyAny>) -> PyResult<NestConfig> {
    let json: Value = pythonize::depythonize(obj).map_err(value_err)?;
    if !json.is_object() {
        return Err(PyValueError::new_err("config must be a mapping"));
    }
    serde_json::from_value(map_keys(json, &to_camel)).map_err(value_err)
}

/// Coerce the `config` argument of [`nest`]: `None`, a `NestConfig`, or a dict.
fn coerce_config(obj: Option<&Bound<'_, PyAny>>) -> PyResult<NestConfig> {
    match obj {
        None => Ok(NestConfig::default()),
        Some(o) if o.is_none() => Ok(NestConfig::default()),
        Some(o) => {
            if let Ok(cfg) = o.cast::<PyNestConfig>() {
                Ok(cfg.borrow().inner.clone())
            } else {
                config_from_mapping(o)
            }
        }
    }
}

fn coerce_tabs(obj: Option<&Bound<'_, PyAny>>) -> PyResult<Option<TabConfig>> {
    let Some(o) = obj else { return Ok(None) };
    if o.is_none() {
        return Ok(None);
    }
    if let Ok(t) = o.cast::<PyTabConfig>() {
        return Ok(Some(t.borrow().inner.clone()));
    }
    let json: Value = pythonize::depythonize(o).map_err(value_err)?;
    if !json.is_object() {
        return Err(PyValueError::new_err(
            "tabs must be a TabConfig or a mapping",
        ));
    }
    Ok(Some(
        serde_json::from_value(map_keys(json, &to_camel)).map_err(value_err)?,
    ))
}

// ---------------------------------------------------------------------------
// Part
// ---------------------------------------------------------------------------

/// One part to nest: an outer contour plus zero or more holes.
///
/// Build with :meth:`Part.from_polygon` or :meth:`Part.from_dxf`. Geometry is
/// normalized so the outer bounding box sits with its min corner at (0, 0).
#[pyclass(name = "Part", module = "sheetnest", from_py_object)]
#[derive(Clone)]
pub struct PyPart {
    pub(crate) inner: Part,
}

#[pymethods]
impl PyPart {
    /// Build a part from plain vertex rings (straight edges only).
    ///
    /// Rings may be given in either winding order, with or without a repeated
    /// closing vertex. Raises ``ValueError`` on a degenerate ring.
    #[staticmethod]
    #[pyo3(signature = (name, quantity, outer, holes = Vec::new()))]
    fn from_polygon(
        name: &str,
        quantity: u32,
        outer: Vec<(f64, f64)>,
        holes: Vec<Vec<(f64, f64)>>,
    ) -> PyResult<PyPart> {
        let outer: Vec<Pt> = outer.into_iter().map(|(x, y)| Pt::new(x, y)).collect();
        let holes: Vec<Vec<Pt>> = holes
            .into_iter()
            .map(|h| h.into_iter().map(|(x, y)| Pt::new(x, y)).collect())
            .collect();
        let inner = Part::from_polygon(name, quantity, &outer, &holes).map_err(value_err)?;
        Ok(PyPart { inner })
    }

    /// Parse the bytes of a DXF file into parts.
    ///
    /// Returns ``(parts, warnings)``. Raises ``ValueError`` when the bytes are
    /// not a readable DXF.
    #[staticmethod]
    #[pyo3(signature = (data, name, quantity = 1, curve_tolerance = 0.25))]
    fn from_dxf(
        data: &[u8],
        name: &str,
        quantity: u32,
        curve_tolerance: f64,
    ) -> PyResult<(Vec<PyPart>, Vec<String>)> {
        let parsed =
            sheetnest::dxf::parse_dxf(data, name, quantity, curve_tolerance).map_err(value_err)?;
        Ok((
            parsed
                .parts
                .into_iter()
                .map(|inner| PyPart { inner })
                .collect(),
            parsed.warnings,
        ))
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[setter]
    fn set_name(&mut self, name: String) {
        self.inner.name = name;
    }

    #[getter]
    fn quantity(&self) -> u32 {
        self.inner.quantity
    }

    #[setter]
    fn set_quantity(&mut self, quantity: u32) {
        self.inner.quantity = quantity;
    }

    /// Area of the outer contour, mm².
    #[getter]
    fn gross_area(&self) -> f64 {
        self.inner.gross_area
    }

    /// Outer area minus hole areas, mm².
    #[getter]
    fn net_area(&self) -> f64 {
        self.inner.net_area
    }

    /// Bounding box of the part as ``(width, height)`` in mm.
    #[getter]
    fn bbox(&self) -> (f64, f64) {
        let (minx, miny, maxx, maxy) = ring_bbox(&self.inner.outer_poly);
        ((maxx - minx).max(0.0), (maxy - miny).max(0.0))
    }

    /// Number of holes in the part.
    #[getter]
    fn hole_count(&self) -> usize {
        self.inner.holes.len()
    }

    fn __repr__(&self) -> String {
        let (w, h) = self.bbox();
        format!(
            "Part(name={:?}, quantity={}, bbox=({:.3}, {:.3}), net_area={:.3})",
            self.inner.name, self.inner.quantity, w, h, self.inner.net_area
        )
    }
}

// ---------------------------------------------------------------------------
// TabConfig
// ---------------------------------------------------------------------------

/// Micro-joint ("tab") settings: small uncut gaps that keep cut parts from
/// tipping into the cutting head.
#[pyclass(name = "TabConfig", module = "sheetnest", from_py_object)]
#[derive(Clone)]
pub struct PyTabConfig {
    pub(crate) inner: TabConfig,
}

#[pymethods]
impl PyTabConfig {
    #[new]
    #[pyo3(signature = (
        *,
        enabled = None,
        width = None,
        max_spacing = None,
        min_per_contour = None,
        corner_clearance = None,
        min_hole_size = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        enabled: Option<bool>,
        width: Option<f64>,
        max_spacing: Option<f64>,
        min_per_contour: Option<u32>,
        corner_clearance: Option<f64>,
        min_hole_size: Option<f64>,
    ) -> PyTabConfig {
        let mut inner = TabConfig::default();
        if let Some(v) = enabled {
            inner.enabled = v;
        }
        if let Some(v) = width {
            inner.width = v;
        }
        if let Some(v) = max_spacing {
            inner.max_spacing = v;
        }
        if let Some(v) = min_per_contour {
            inner.min_per_contour = v;
        }
        if let Some(v) = corner_clearance {
            inner.corner_clearance = v;
        }
        if let Some(v) = min_hole_size {
            inner.min_hole_size = v;
        }
        PyTabConfig { inner }
    }

    #[getter]
    fn enabled(&self) -> bool {
        self.inner.enabled
    }

    #[setter]
    fn set_enabled(&mut self, v: bool) {
        self.inner.enabled = v;
    }

    #[getter]
    fn width(&self) -> f64 {
        self.inner.width
    }

    #[setter]
    fn set_width(&mut self, v: f64) {
        self.inner.width = v;
    }

    #[getter]
    fn max_spacing(&self) -> f64 {
        self.inner.max_spacing
    }

    #[setter]
    fn set_max_spacing(&mut self, v: f64) {
        self.inner.max_spacing = v;
    }

    #[getter]
    fn min_per_contour(&self) -> u32 {
        self.inner.min_per_contour
    }

    #[setter]
    fn set_min_per_contour(&mut self, v: u32) {
        self.inner.min_per_contour = v;
    }

    #[getter]
    fn corner_clearance(&self) -> f64 {
        self.inner.corner_clearance
    }

    #[setter]
    fn set_corner_clearance(&mut self, v: f64) {
        self.inner.corner_clearance = v;
    }

    #[getter]
    fn min_hole_size(&self) -> f64 {
        self.inner.min_hole_size
    }

    #[setter]
    fn set_min_hole_size(&mut self, v: f64) {
        self.inner.min_hole_size = v;
    }

    /// Build a ``TabConfig`` from a mapping (camelCase or snake_case keys).
    #[staticmethod]
    fn from_dict(d: &Bound<'_, PyAny>) -> PyResult<PyTabConfig> {
        let inner = coerce_tabs(Some(d))?.unwrap_or_default();
        Ok(PyTabConfig { inner })
    }

    /// The settings as a plain dict with snake_case keys.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        serde_to_py(py, &self.inner)
    }

    fn __repr__(&self) -> String {
        format!(
            "TabConfig(enabled={}, width={}, max_spacing={}, min_per_contour={}, \
             corner_clearance={}, min_hole_size={})",
            if self.inner.enabled { "True" } else { "False" },
            self.inner.width,
            self.inner.max_spacing,
            self.inner.min_per_contour,
            self.inner.corner_clearance,
            self.inner.min_hole_size
        )
    }
}

// ---------------------------------------------------------------------------
// NestConfig
// ---------------------------------------------------------------------------

/// Run settings for :func:`nest`. Every field is optional and falls back to the
/// engine default.
#[pyclass(name = "NestConfig", module = "sheetnest", from_py_object)]
#[derive(Clone)]
pub struct PyNestConfig {
    pub(crate) inner: NestConfig,
}

#[pymethods]
impl PyNestConfig {
    #[new]
    #[pyo3(signature = (
        *,
        sheet_width = None,
        sheet_height = None,
        auto_width = None,
        spacing = None,
        margin = None,
        rotation_mode = None,
        rotation_step_deg = None,
        curve_tolerance = None,
        time_limit_ms = None,
        population = None,
        mutation_rate = None,
        stale_generations = None,
        seed = None,
        tabs = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        sheet_width: Option<f64>,
        sheet_height: Option<f64>,
        auto_width: Option<bool>,
        spacing: Option<f64>,
        margin: Option<f64>,
        rotation_mode: Option<&str>,
        rotation_step_deg: Option<f64>,
        curve_tolerance: Option<f64>,
        time_limit_ms: Option<u64>,
        population: Option<usize>,
        mutation_rate: Option<f64>,
        stale_generations: Option<u32>,
        seed: Option<u64>,
        tabs: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyNestConfig> {
        let mut inner = NestConfig::default();
        if let Some(v) = sheet_width {
            inner.sheet_width = v;
        }
        if let Some(v) = sheet_height {
            inner.sheet_height = v;
        }
        if let Some(v) = auto_width {
            inner.auto_width = v;
        }
        if let Some(v) = spacing {
            inner.spacing = v;
        }
        if let Some(v) = margin {
            inner.margin = v;
        }
        if let Some(v) = rotation_mode {
            inner.rotation_mode = rotation_mode_from_str(v)?;
        }
        if let Some(v) = rotation_step_deg {
            inner.rotation_step_deg = v;
        }
        if let Some(v) = curve_tolerance {
            inner.curve_tolerance = v;
        }
        if let Some(v) = time_limit_ms {
            inner.time_limit_ms = v;
        }
        if let Some(v) = population {
            inner.population = v;
        }
        if let Some(v) = mutation_rate {
            inner.mutation_rate = v;
        }
        if let Some(v) = stale_generations {
            inner.stale_generations = v;
        }
        if seed.is_some() {
            inner.seed = seed;
        }
        if let Some(t) = coerce_tabs(tabs)? {
            inner.tabs = t;
        }
        Ok(PyNestConfig { inner })
    }

    /// Build a config from a mapping. Keys may be camelCase or snake_case;
    /// missing keys fall back to the engine defaults.
    #[staticmethod]
    fn from_dict(d: &Bound<'_, PyAny>) -> PyResult<PyNestConfig> {
        Ok(PyNestConfig {
            inner: config_from_mapping(d)?,
        })
    }

    /// The config as a plain dict with snake_case keys.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        serde_to_py(py, &self.inner)
    }

    #[getter]
    fn sheet_width(&self) -> f64 {
        self.inner.sheet_width
    }

    #[setter]
    fn set_sheet_width(&mut self, v: f64) {
        self.inner.sheet_width = v;
    }

    #[getter]
    fn sheet_height(&self) -> f64 {
        self.inner.sheet_height
    }

    #[setter]
    fn set_sheet_height(&mut self, v: f64) {
        self.inner.sheet_height = v;
    }

    #[getter]
    fn auto_width(&self) -> bool {
        self.inner.auto_width
    }

    #[setter]
    fn set_auto_width(&mut self, v: bool) {
        self.inner.auto_width = v;
    }

    #[getter]
    fn spacing(&self) -> f64 {
        self.inner.spacing
    }

    #[setter]
    fn set_spacing(&mut self, v: f64) {
        self.inner.spacing = v;
    }

    #[getter]
    fn margin(&self) -> f64 {
        self.inner.margin
    }

    #[setter]
    fn set_margin(&mut self, v: f64) {
        self.inner.margin = v;
    }

    #[getter]
    fn rotation_step_deg(&self) -> f64 {
        self.inner.rotation_step_deg
    }

    #[setter]
    fn set_rotation_step_deg(&mut self, v: f64) {
        self.inner.rotation_step_deg = v;
    }

    #[getter]
    fn curve_tolerance(&self) -> f64 {
        self.inner.curve_tolerance
    }

    #[setter]
    fn set_curve_tolerance(&mut self, v: f64) {
        self.inner.curve_tolerance = v;
    }

    #[getter]
    fn time_limit_ms(&self) -> u64 {
        self.inner.time_limit_ms
    }

    #[setter]
    fn set_time_limit_ms(&mut self, v: u64) {
        self.inner.time_limit_ms = v;
    }

    #[getter]
    fn population(&self) -> usize {
        self.inner.population
    }

    #[setter]
    fn set_population(&mut self, v: usize) {
        self.inner.population = v;
    }

    #[getter]
    fn mutation_rate(&self) -> f64 {
        self.inner.mutation_rate
    }

    #[setter]
    fn set_mutation_rate(&mut self, v: f64) {
        self.inner.mutation_rate = v;
    }

    #[getter]
    fn stale_generations(&self) -> u32 {
        self.inner.stale_generations
    }

    #[setter]
    fn set_stale_generations(&mut self, v: u32) {
        self.inner.stale_generations = v;
    }

    #[getter]
    fn rotation_mode(&self) -> &'static str {
        rotation_mode_str(self.inner.rotation_mode)
    }

    #[setter]
    fn set_rotation_mode(&mut self, v: &str) -> PyResult<()> {
        self.inner.rotation_mode = rotation_mode_from_str(v)?;
        Ok(())
    }

    #[getter]
    fn seed(&self) -> Option<u64> {
        self.inner.seed
    }

    #[setter]
    fn set_seed(&mut self, v: Option<u64>) {
        self.inner.seed = v;
    }

    #[getter]
    fn tabs(&self) -> PyTabConfig {
        PyTabConfig {
            inner: self.inner.tabs.clone(),
        }
    }

    #[setter]
    fn set_tabs(&mut self, v: &Bound<'_, PyAny>) -> PyResult<()> {
        self.inner.tabs = coerce_tabs(Some(v))?.unwrap_or_default();
        Ok(())
    }

    fn __repr__(&self) -> String {
        let c = &self.inner;
        format!(
            "NestConfig(sheet_width={}, sheet_height={}, auto_width={}, spacing={}, margin={}, \
             rotation_mode={:?}, time_limit_ms={}, population={}, stale_generations={}, seed={})",
            c.sheet_width,
            c.sheet_height,
            if c.auto_width { "True" } else { "False" },
            c.spacing,
            c.margin,
            rotation_mode_str(c.rotation_mode),
            c.time_limit_ms,
            c.population,
            c.stale_generations,
            match c.seed {
                Some(s) => s.to_string(),
                None => "None".to_string(),
            }
        )
    }
}

// ---------------------------------------------------------------------------
// Progress / Placement / Stats / Solution
// ---------------------------------------------------------------------------

/// Snapshot handed to the ``on_progress`` callback once per generation.
#[pyclass(name = "Progress", module = "sheetnest", frozen, get_all)]
pub struct PyProgress {
    pub generation: u32,
    /// Lower is better: strip area consumed, mm², plus penalties.
    pub best_fitness: f64,
    pub best_utilization: f64,
    pub elapsed_ms: u64,
}

#[pymethods]
impl PyProgress {
    fn __repr__(&self) -> String {
        format!(
            "Progress(generation={}, best_fitness={:.3}, best_utilization={:.4}, elapsed_ms={})",
            self.generation, self.best_fitness, self.best_utilization, self.elapsed_ms
        )
    }
}

/// One placed part instance: rotate the part by ``rotation_deg`` about the
/// origin, then translate by ``(dx, dy)``.
#[pyclass(name = "Placement", module = "sheetnest", frozen, get_all)]
pub struct PyPlacement {
    /// Index of the part in the list passed to :func:`nest`.
    pub part_id: usize,
    pub part_name: String,
    pub instance: u32,
    pub sheet: usize,
    pub rotation_deg: f64,
    pub dx: f64,
    pub dy: f64,
}

#[pymethods]
impl PyPlacement {
    fn __repr__(&self) -> String {
        format!(
            "Placement(part_id={}, part_name={:?}, instance={}, sheet={}, rotation_deg={}, \
             dx={:.3}, dy={:.3})",
            self.part_id,
            self.part_name,
            self.instance,
            self.sheet,
            self.rotation_deg,
            self.dx,
            self.dy
        )
    }
}

/// Summary of a finished run.
#[pyclass(name = "Stats", module = "sheetnest", frozen, get_all)]
pub struct PyStats {
    /// One of ``"time_limit"``, ``"stale"``, ``"cancelled"``, ``"empty"``.
    pub stop_reason: String,
    pub sheets_used: usize,
    /// Length of stock consumed along X, mm — the headline metric.
    pub used_width: f64,
    pub utilization: f64,
    pub strip_utilization: f64,
    pub generations: u32,
    pub elapsed_ms: u64,
    pub placed: usize,
    pub total: usize,
}

#[pymethods]
impl PyStats {
    fn __repr__(&self) -> String {
        format!(
            "Stats(stop_reason={:?}, sheets_used={}, used_width={:.3}, utilization={:.4}, \
             strip_utilization={:.4}, generations={}, elapsed_ms={}, placed={}, total={})",
            self.stop_reason,
            self.sheets_used,
            self.used_width,
            self.utilization,
            self.strip_utilization,
            self.generations,
            self.elapsed_ms,
            self.placed,
            self.total
        )
    }
}

fn placement_py(p: &Placement) -> PyPlacement {
    PyPlacement {
        part_id: p.part_id,
        part_name: p.part_name.clone(),
        instance: p.instance,
        sheet: p.sheet,
        rotation_deg: p.rotation_deg,
        dx: p.dx,
        dy: p.dy,
    }
}

/// The result of a run. Holds copies of the parts and config, so it can render
/// itself to DXF or SVG with no further arguments.
#[pyclass(name = "Solution", module = "sheetnest", frozen)]
pub struct PySolution {
    inner: NestSolution,
    parts: Vec<Part>,
    config: NestConfig,
}

#[pymethods]
impl PySolution {
    #[getter]
    fn placements(&self) -> Vec<PyPlacement> {
        self.inner.placements.iter().map(placement_py).collect()
    }

    #[getter]
    fn stats(&self) -> PyStats {
        let s = &self.inner.stats;
        PyStats {
            stop_reason: stop_reason_str(s.stop_reason).to_string(),
            sheets_used: s.sheets_used,
            used_width: s.used_width,
            utilization: s.utilization,
            strip_utilization: s.strip_utilization,
            generations: s.generations,
            elapsed_ms: s.elapsed_ms,
            placed: s.placed,
            total: s.total,
        }
    }

    /// Width the placements are laid out on, mm.
    #[getter]
    fn sheet_width(&self) -> f64 {
        self.inner.sheet_width
    }

    #[getter]
    fn sheet_height(&self) -> f64 {
        self.inner.sheet_height
    }

    #[getter]
    fn warnings(&self) -> Vec<String> {
        self.inner.warnings.clone()
    }

    /// The config this solution was produced with.
    #[getter]
    fn config(&self) -> PyNestConfig {
        PyNestConfig {
            inner: self.config.clone(),
        }
    }

    /// The parts this solution was produced from, in ``part_id`` order.
    #[getter]
    fn parts(&self) -> Vec<PyPart> {
        self.parts
            .iter()
            .map(|p| PyPart { inner: p.clone() })
            .collect()
    }

    /// The whole solution as plain dicts and lists, with snake_case keys.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        serde_to_py(py, &self.inner)
    }

    /// Serialize the layout as a DXF file for the cutter: every sheet side by
    /// side along +X, sheet outlines on layer ``SHEET``, cut geometry on
    /// ``CUT``.
    fn to_dxf<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let bytes =
            sheetnest::to_dxf(&self.inner, &self.parts, &self.config).map_err(runtime_err)?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// Render one sheet as an SVG document.
    #[pyo3(signature = (sheet = 0))]
    fn to_svg(&self, sheet: usize) -> String {
        sheetnest::to_svg(&self.inner, &self.parts, &self.config, sheet)
    }

    /// Render every sheet, stacked vertically in one SVG.
    fn to_svg_all(&self) -> String {
        sheetnest::to_svg_all(&self.inner, &self.parts, &self.config)
    }

    fn __repr__(&self) -> String {
        let s = &self.inner.stats;
        format!(
            "Solution(placed={}/{}, sheets_used={}, used_width={:.1}, utilization={:.3}, \
             stop_reason={:?})",
            s.placed,
            s.total,
            s.sheets_used,
            s.used_width,
            s.utilization,
            stop_reason_str(s.stop_reason)
        )
    }
}

// ---------------------------------------------------------------------------
// nest()
// ---------------------------------------------------------------------------

/// Shared state between the Rust hooks (which run with the GIL released) and
/// the `nest` call that has to re-raise afterwards.
struct HookState {
    stop: AtomicBool,
    err: Mutex<Option<PyErr>>,
}

impl HookState {
    fn fail(&self, e: PyErr) {
        let mut slot = self.err.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(e);
        }
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Nest `parts` onto sheets described by `config`.
///
/// Returns the best layout found within the time limit, or earlier if the run
/// goes stale or a hook asks it to stop. The genetic algorithm runs with the
/// GIL released; ``on_progress`` and ``should_stop`` are called once per
/// generation with the GIL re-acquired. Ctrl-C is checked once per generation
/// and raises ``KeyboardInterrupt``.
#[pyfunction]
#[pyo3(signature = (parts, config = None, *, on_progress = None, should_stop = None))]
fn nest(
    py: Python<'_>,
    parts: Vec<Py<PyPart>>,
    config: Option<&Bound<'_, PyAny>>,
    on_progress: Option<Py<PyAny>>,
    should_stop: Option<Py<PyAny>>,
) -> PyResult<PySolution> {
    let cfg = coerce_config(config)?;
    let rust_parts: Vec<Part> = parts.iter().map(|p| p.borrow(py).inner.clone()).collect();

    let state = Arc::new(HookState {
        stop: AtomicBool::new(false),
        err: Mutex::new(None),
    });

    let mut hooks = Hooks::new();

    if let Some(cb) = on_progress {
        let st = Arc::clone(&state);
        hooks = hooks.on_progress(move |p: &Progress| {
            if st.stop.load(Ordering::SeqCst) {
                return;
            }
            Python::attach(|py| {
                let snapshot = PyProgress {
                    generation: p.generation,
                    best_fitness: p.best_fitness,
                    best_utilization: p.best_utilization,
                    elapsed_ms: p.elapsed_ms,
                };
                let res = Py::new(py, snapshot).and_then(|arg| cb.call1(py, (arg,)));
                if let Err(e) = res {
                    st.fail(e);
                }
            });
        });
    }

    // Always registered: it is what makes Ctrl-C work.
    {
        let st = Arc::clone(&state);
        hooks = hooks.should_stop(move || {
            if st.stop.load(Ordering::SeqCst) {
                return true;
            }
            Python::attach(|py| {
                if let Err(e) = py.check_signals() {
                    st.fail(e);
                    return true;
                }
                let Some(cb) = &should_stop else { return false };
                match cb.bind(py).call0().and_then(|r| r.is_truthy()) {
                    Ok(v) => v,
                    Err(e) => {
                        st.fail(e);
                        true
                    }
                }
            })
        });
    }

    let solution = py.detach(|| sheetnest::nest(&rust_parts, &cfg, hooks));

    // A hook that raised wins over whatever the engine returned.
    if let Some(e) = state.err.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(e);
    }

    Ok(PySolution {
        inner: solution.map_err(runtime_err)?,
        parts: rust_parts,
        config: cfg,
    })
}

// ---------------------------------------------------------------------------
// module
// ---------------------------------------------------------------------------

#[pymodule]
fn _sheetnest(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_class::<PyPart>()?;
    m.add_class::<PyTabConfig>()?;
    m.add_class::<PyNestConfig>()?;
    m.add_class::<PyProgress>()?;
    m.add_class::<PyPlacement>()?;
    m.add_class::<PyStats>()?;
    m.add_class::<PySolution>()?;
    m.add_function(wrap_pyfunction!(nest, m)?)?;
    Ok(())
}
