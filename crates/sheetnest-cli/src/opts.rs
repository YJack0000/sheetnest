//! Options shared by `nest` and `bench`, and the job-file loader.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use sheetnest::dxf::parse_dxf;
use sheetnest::model::{NestConfig, Part, RotationMode};

/// How the nester is allowed to turn a part.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum RotationArg {
    /// Quarter turns only: 0, 90, 180, 270 degrees. Right for rolled or
    /// brushed stock where the grain has to stay square to the sheet.
    Orthogonal,
    /// Any multiple of `--rotation-step`. Packs tighter, but the parts come
    /// off the table at odd angles.
    Free,
}

impl From<RotationArg> for RotationMode {
    fn from(r: RotationArg) -> Self {
        match r {
            RotationArg::Orthogonal => RotationMode::Orthogonal,
            RotationArg::Free => RotationMode::Free,
        }
    }
}

/// Cutting parameters. Everything is millimetres and degrees.
///
/// Every value has a sane default, so you only pass what differs from the
/// shop's usual setup.
#[derive(Args, Debug, Clone)]
pub struct NestOpts {
    /// Sheet size as WIDTHxHEIGHT in mm, e.g. `2500x1250`. Default 1829x914
    /// (a 6ft x 3ft plate).
    #[arg(long, value_name = "WxH", value_parser = parse_sheet)]
    pub sheet: Option<(f64, f64)>,

    /// Treat the stock as an endless strip (a coil) and let the nester decide
    /// how far along it to cut. `--sheet` then only sets the strip height.
    #[arg(long)]
    pub auto_width: bool,

    /// Smallest gap left between two parts, mm. Give the torch or blade room.
    /// Default 2.
    #[arg(long, value_name = "MM")]
    pub spacing: Option<f64>,

    /// Smallest gap left between a part and the edge of the sheet, mm.
    /// Default 5.
    #[arg(long, value_name = "MM")]
    pub margin: Option<f64>,

    /// Which turns the nester may use for a part. Default orthogonal.
    #[arg(long, value_name = "MODE", value_enum)]
    pub rotation: Option<RotationArg>,

    /// Angle step for `--rotation free`, degrees. Smaller packs tighter and
    /// runs slower. Default 15.
    #[arg(long, value_name = "DEG")]
    pub rotation_step: Option<f64>,

    /// How far a straight line may stray from the true curve when arcs and
    /// splines are flattened for the packing maths, mm. Output arcs stay
    /// true arcs regardless. Default 0.25.
    #[arg(long, value_name = "MM")]
    pub curve_tolerance: Option<f64>,

    /// Hard stop on the search, milliseconds. The best layout so far is kept.
    /// Default 20000.
    #[arg(long, value_name = "MS")]
    pub time_limit_ms: Option<u64>,

    /// How many candidate layouts the search keeps alive at once. Default 15.
    #[arg(long, value_name = "N")]
    pub population: Option<usize>,

    /// How often the search shuffles a layout at random, 0.0 to 1.0.
    /// Default 0.10.
    #[arg(long, value_name = "RATE")]
    pub mutation_rate: Option<f64>,

    /// Give up after this many rounds with no improvement. Lower finishes
    /// sooner, higher squeezes out a little more stock. Default 600.
    #[arg(long, value_name = "N")]
    pub stale_generations: Option<u32>,

    /// Fix the random seed so the same job produces the same layout twice.
    /// Only bites when the run ends on `--stale-generations`; a run that hits
    /// the clock stops at a machine-dependent point.
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,

    /// Leave micro-joints (tabs) so cut parts stay tacked into the sheet
    /// instead of tipping into the cutting head.
    #[arg(long)]
    pub tabs: bool,

    /// Width of each micro-joint, mm. Default 0.3. Implies `--tabs`.
    #[arg(long, value_name = "MM")]
    pub tab_width: Option<f64>,

    /// Longest run of cut allowed between two micro-joints, mm. Default 250.
    /// Implies `--tabs`.
    #[arg(long, value_name = "MM")]
    pub tab_spacing: Option<f64>,

    /// Start from a saved settings file (JSON, camelCase keys). Any flag you
    /// also pass on the command line wins over the file.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
}

impl NestOpts {
    /// Fold the flags (and the optional `--config` file) into a config.
    pub fn to_config(&self) -> Result<NestConfig> {
        let mut cfg = match &self.config {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("reading config {}", p.display()))?;
                serde_json::from_str(&text)
                    .with_context(|| format!("parsing config {}", p.display()))?
            }
            None => NestConfig::default(),
        };

        if let Some((w, h)) = self.sheet {
            cfg.sheet_width = w;
            cfg.sheet_height = h;
        }
        if self.auto_width {
            cfg.auto_width = true;
        }
        if let Some(v) = self.spacing {
            cfg.spacing = v;
        }
        if let Some(v) = self.margin {
            cfg.margin = v;
        }
        if let Some(v) = self.rotation {
            cfg.rotation_mode = v.into();
        }
        if let Some(v) = self.rotation_step {
            cfg.rotation_step_deg = v;
        }
        if let Some(v) = self.curve_tolerance {
            cfg.curve_tolerance = v;
        }
        if let Some(v) = self.time_limit_ms {
            cfg.time_limit_ms = v;
        }
        if let Some(v) = self.population {
            cfg.population = v;
        }
        if let Some(v) = self.mutation_rate {
            cfg.mutation_rate = v;
        }
        if let Some(v) = self.stale_generations {
            cfg.stale_generations = v;
        }
        if let Some(v) = self.seed {
            cfg.seed = Some(v);
        }
        if self.tabs {
            cfg.tabs.enabled = true;
        }
        if let Some(v) = self.tab_width {
            cfg.tabs.width = v;
            cfg.tabs.enabled = true;
        }
        if let Some(v) = self.tab_spacing {
            cfg.tabs.max_spacing = v;
            cfg.tabs.enabled = true;
        }

        check(cfg.sheet_width > 0.0, "--sheet width must be above zero")?;
        check(cfg.sheet_height > 0.0, "--sheet height must be above zero")?;
        check(cfg.spacing >= 0.0, "--spacing cannot be negative")?;
        check(cfg.margin >= 0.0, "--margin cannot be negative")?;
        check(
            cfg.curve_tolerance > 0.0,
            "--curve-tolerance must be above zero",
        )?;
        check(cfg.population >= 2, "--population must be at least 2")?;
        check(
            (0.0..=1.0).contains(&cfg.mutation_rate),
            "--mutation-rate must be between 0.0 and 1.0",
        )?;
        check(
            cfg.rotation_step_deg > 0.0,
            "--rotation-step must be above zero",
        )?;
        check(cfg.tabs.width >= 0.0, "--tab-width cannot be negative")?;
        check(
            cfg.tabs.max_spacing > 0.0,
            "--tab-spacing must be above zero",
        )?;

        Ok(cfg)
    }
}

fn check(ok: bool, msg: &str) -> Result<()> {
    if ok { Ok(()) } else { bail!("{msg}") }
}

/// Parse `1829x914` (or `1829X914`) into width and height.
pub fn parse_sheet(s: &str) -> Result<(f64, f64), String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected WIDTHxHEIGHT, e.g. 1829x914 (got `{s}`)"))?;
    let w: f64 = w
        .trim()
        .parse()
        .map_err(|_| format!("`{w}` is not a number"))?;
    let h: f64 = h
        .trim()
        .parse()
        .map_err(|_| format!("`{h}` is not a number"))?;
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return Err(format!("sheet size must be positive (got `{s}`)"));
    }
    Ok((w, h))
}

/// One drawing on the job list, with how many of it to cut.
#[derive(Debug, Clone)]
pub struct PartSpec {
    pub path: PathBuf,
    pub quantity: u32,
}

/// Parse `bracket.dxf` or `bracket.dxf:12`.
///
/// The count is whatever follows the last colon, when it reads as a whole
/// number; anything else is taken as part of the path, so Windows drive
/// letters survive.
pub fn parse_part_spec(s: &str) -> Result<PartSpec, String> {
    if s.is_empty() {
        return Err("empty file name".to_string());
    }
    if let Some((path, qty)) = s.rsplit_once(':')
        && let Ok(quantity) = qty.parse::<u32>()
    {
        if quantity == 0 {
            return Err(format!("quantity must be at least 1 (got `{s}`)"));
        }
        if path.is_empty() {
            return Err(format!("missing file name before `:` in `{s}`"));
        }
        return Ok(PartSpec {
            path: PathBuf::from(path),
            quantity,
        });
    }
    Ok(PartSpec {
        path: PathBuf::from(s),
        quantity: 1,
    })
}

/// Read every drawing on the job list into parts, keeping part names unique.
///
/// Returns the parts plus any warnings the DXF reader raised.
pub fn load_parts(specs: &[PartSpec], curve_tol: f64) -> Result<(Vec<Part>, Vec<String>)> {
    let mut parts: Vec<Part> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut seen: HashMap<String, u32> = HashMap::new();

    for spec in specs {
        let bytes = std::fs::read(&spec.path)
            .with_context(|| format!("reading {}", spec.path.display()))?;
        let base = file_label(&spec.path);
        let count = seen.entry(base.clone()).or_insert(0);
        *count += 1;
        let name = if *count == 1 {
            base
        } else {
            format!("{base}#{count}")
        };

        let parsed = parse_dxf(&bytes, &name, spec.quantity, curve_tol)
            .with_context(|| format!("reading {}", spec.path.display()))?;
        if parsed.parts.is_empty() {
            bail!(
                "{}: no closed outlines found - the drawing needs closed contours to nest",
                spec.path.display()
            );
        }
        parts.extend(parsed.parts);
        warnings.extend(parsed.warnings);
    }

    if parts.is_empty() {
        bail!("nothing to nest");
    }
    Ok((parts, warnings))
}

/// Every `.dxf` in a directory, sorted, as a job list at `quantity` each.
pub fn load_dir_specs(dir: &Path, quantity: u32) -> Result<Vec<PartSpec>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dxf")))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no .dxf files in {}", dir.display());
    }
    Ok(files
        .into_iter()
        .map(|path| PartSpec { path, quantity })
        .collect())
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}
