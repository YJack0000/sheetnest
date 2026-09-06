//! `sheetnest bench` - nest a whole folder of drawings and report the numbers.
//!
//! Runs a directory of DXF parts as one job and reports the standard nesting
//! metrics so improvements can be tracked run over run:
//!
//! - sheets used
//! - used width         = stock consumed along X (what the search minimises)
//! - utilization        = net part area / (sheets * sheet area)
//! - strip utilization  = net part area / area actually spanned
//! - rounds evolved and wall time

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::nest::run_nest;
use crate::opts::{NestOpts, load_dir_specs, load_parts};

/// Nest every DXF in a folder as one job and print the numbers.
///
/// Handy for comparing two builds, two machines, or two sets of cutting
/// parameters on the same pile of parts.
#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Folder of DXF drawings. Every `.dxf` in it joins the job.
    #[arg(value_name = "DIR")]
    pub dir: PathBuf,

    /// How many of each drawing to cut.
    #[arg(value_name = "QTY", default_value_t = 1)]
    pub quantity: u32,

    #[command(flatten)]
    pub opts: NestOpts,
}

pub fn run(args: BenchArgs) -> Result<()> {
    let cfg = args.opts.to_config()?;
    let specs = load_dir_specs(&args.dir, args.quantity)?;
    let (parts, warnings) = load_parts(&specs, cfg.curve_tolerance)?;

    let total_instances: u32 = parts.iter().map(|p| p.quantity).sum();
    println!(
        "bench: {} parts x qty {} = {} instances | sheet {}x{} | {:?} | budget {}ms",
        parts.len(),
        args.quantity,
        total_instances,
        cfg.sheet_width,
        cfg.sheet_height,
        cfg.rotation_mode,
        cfg.time_limit_ms
    );
    for w in &warnings {
        println!("  warn: {w}");
    }

    let t0 = std::time::Instant::now();
    let sol = run_nest(&parts, &cfg, true)?;
    let wall = t0.elapsed().as_millis();

    println!("--------------------------------------------------------------");
    println!("placed          : {}/{}", sol.stats.placed, sol.stats.total);
    println!("sheets used     : {}", sol.stats.sheets_used);
    println!("used width      : {:.1} mm", sol.stats.used_width);
    println!("utilization     : {:.1}%", sol.stats.utilization * 100.0);
    println!(
        "strip util      : {:.1}%",
        sol.stats.strip_utilization * 100.0
    );
    println!("rounds          : {}", sol.stats.generations);
    println!("wall time       : {wall} ms");
    for w in &sol.warnings {
        println!("warn            : {w}");
    }
    Ok(())
}
