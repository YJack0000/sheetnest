//! `sheetnest nest` - pack drawings onto stock and write the cutting file.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use sheetnest::model::{NestConfig, NestSolution, Part, StopReason};
use sheetnest::{Hooks, nest, to_dxf, to_svg_all};

use crate::opts::{NestOpts, PartSpec, load_parts, parse_part_spec};

/// Nest one or more DXF drawings onto sheets and write the result.
///
/// Give each drawing once, with the number of copies after a colon:
///
///     sheetnest nest bracket.dxf:12 gusset.dxf:4 -o job.dxf
#[derive(Args, Debug)]
pub struct NestArgs {
    /// Drawings to cut, as `file.dxf` or `file.dxf:COUNT` (default 1 each).
    #[arg(value_name = "FILE[:QTY]", required = true, value_parser = parse_part_spec)]
    pub files: Vec<PartSpec>,

    /// Where to write the nested DXF for the cutter.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Also write a picture of every sheet, stacked top to bottom, as SVG.
    #[arg(long, value_name = "FILE")]
    pub svg: Option<PathBuf>,

    /// Print the full result as JSON instead of the readable summary.
    #[arg(long)]
    pub json: bool,

    /// Say nothing but errors: no progress, no summary.
    #[arg(short, long)]
    pub quiet: bool,

    #[command(flatten)]
    pub opts: NestOpts,
}

pub fn run(args: NestArgs) -> Result<()> {
    if args.output.is_none() && args.svg.is_none() && !args.json {
        bail!("nothing to write: pass -o <FILE>, --svg <FILE>, or --json");
    }

    let cfg = args.opts.to_config()?;
    let (parts, read_warnings) = load_parts(&args.files, cfg.curve_tolerance)?;

    let progress = !args.quiet && !args.json;
    if progress {
        for w in &read_warnings {
            eprintln!("warning: {w}");
        }
    }

    let solution = run_nest(&parts, &cfg, progress)?;

    if let Some(path) = &args.output {
        let bytes = to_dxf(&solution, &parts, &cfg).context("writing the nested DXF")?;
        write_file(path, &bytes)?;
    }
    if let Some(path) = &args.svg {
        let svg = to_svg_all(&solution, &parts, &cfg);
        write_file(path, svg.as_bytes())?;
    }

    if args.json {
        let mut out = serde_json::to_string_pretty(&solution)?;
        out.push('\n');
        print!("{out}");
    } else if !args.quiet {
        print_summary(&solution, &parts, &read_warnings);
    }
    Ok(())
}

/// Run the optimizer, ticking a one-line progress report on stderr.
pub fn run_nest(parts: &[Part], cfg: &NestConfig, progress: bool) -> Result<NestSolution> {
    let mut hooks = Hooks::new();
    if progress {
        let tty = std::io::stderr().is_terminal();
        let mut last = Instant::now() - Duration::from_secs(1);
        hooks = hooks.on_progress(move |p| {
            if last.elapsed() < Duration::from_millis(500) {
                return;
            }
            last = Instant::now();
            let mut err = std::io::stderr().lock();
            let line = format!(
                "  round {:>5} | best fill {:>5.1}% | {:>5.1}s",
                p.generation,
                p.best_utilization * 100.0,
                p.elapsed_ms as f64 / 1000.0
            );
            let _ = if tty {
                write!(err, "\r{line}\x1b[K")
            } else {
                writeln!(err, "{line}")
            };
            let _ = err.flush();
        });
    }

    let solution = nest(parts, cfg, hooks).context("nesting failed")?;

    if progress && std::io::stderr().is_terminal() {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[K");
        let _ = err.flush();
    }
    Ok(solution)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn stop_reason_text(r: StopReason) -> &'static str {
    match r {
        StopReason::TimeLimit => "hit the time limit",
        StopReason::Stale => "stopped improving",
        StopReason::Cancelled => "cancelled",
        StopReason::Empty => "nothing to place",
    }
}

/// The readable end-of-run report.
pub fn print_summary(solution: &NestSolution, parts: &[Part], read_warnings: &[String]) {
    let s = &solution.stats;
    println!("placed          : {}/{}", s.placed, s.total);
    println!("sheets used     : {}", s.sheets_used);
    println!("used width      : {:.1} mm", s.used_width);
    println!("utilization     : {:.1}%", s.utilization * 100.0);
    println!("strip util      : {:.1}%", s.strip_utilization * 100.0);
    println!("rounds          : {}", s.generations);
    println!("elapsed         : {} ms", s.elapsed_ms);
    println!("stopped because : {}", stop_reason_text(s.stop_reason));

    if s.placed < s.total {
        let mut placed_per_part = vec![0u32; parts.len()];
        for p in &solution.placements {
            if let Some(slot) = placed_per_part.get_mut(p.part_id) {
                *slot += 1;
            }
        }
        println!("left over (too big for the sheet, or no room left):");
        for (part, &done) in parts.iter().zip(&placed_per_part) {
            if done < part.quantity {
                println!(
                    "  {}: {} of {} not placed",
                    part.name,
                    part.quantity - done,
                    part.quantity
                );
            }
        }
    }

    for w in read_warnings.iter().chain(solution.warnings.iter()) {
        println!("warning         : {w}");
    }
}
