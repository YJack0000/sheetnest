//! Nesting benchmark harness. Runs a directory of DXF parts as one job and
//! reports the standard nesting metrics so improvements can be tracked
//! run-over-run (do NOT compare against the old SVGnest pipeline as ground
//! truth — evaluate on these numbers plus the validate example).
//!
//! Metrics:
//! - sheets used
//! - used width         = stock consumed along X (what the fitness minimises)
//! - utilization        = net part area / (sheets * sheet area)
//! - strip utilization  = net part area / area actually spanned (SVGnest-style)
//! - generations evolved and wall time
//!
//! Usage:
//!   cargo run --release --example bench -- <dir> [qty=1] [timeLimitMs=20000] [rotationMode=orthogonal|free] [sheetW=1829] [sheetH=914] [staleGenerations=300] [auto]
//!
//! Passing `auto` as the last argument ignores sheetW and lets the nester
//! minimise the width itself (one unbounded strip).

use sheetnest::model::{NestConfig, RotationMode};
use sheetnest::{Hooks, dxf, nest};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| {
        eprintln!("usage: bench <dir> [qty] [timeLimitMs] [rotationMode] [sheetW] [sheetH]");
        std::process::exit(2);
    });
    let qty: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let time_limit: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let rotation_mode = match args.next().as_deref() {
        Some("free") => RotationMode::Free,
        _ => RotationMode::Orthogonal,
    };
    let sheet_w: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1829.0);
    let sheet_h: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(914.0);
    let stale: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);
    // "auto" as the sheet width asks the nester to find the width itself.
    let auto_width = args.next().as_deref() == Some("auto");

    let cfg = NestConfig {
        sheet_width: sheet_w,
        sheet_height: sheet_h,
        time_limit_ms: time_limit,
        rotation_mode,
        stale_generations: stale,
        auto_width,
        ..NestConfig::default()
    };

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dxf")))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .dxf files in {dir}");

    let mut parts = Vec::new();
    let mut warnings = Vec::new();
    for f in &files {
        let bytes = std::fs::read(f).expect("read");
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        match dxf::parse_dxf(&bytes, &name, qty, cfg.curve_tolerance) {
            Ok(mut parsed) => {
                parts.append(&mut parsed.parts);
                warnings.extend(parsed.warnings);
            }
            Err(e) => eprintln!("skip {name}: {e}"),
        }
    }
    let total_instances: u32 = parts.iter().map(|p| p.quantity).sum();
    println!(
        "bench: {} parts x qty {} = {} instances | sheet {}x{} | {:?} | budget {}ms",
        parts.len(),
        qty,
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
    let sol = nest(&parts, &cfg, Hooks::default()).expect("nest failed");
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
    println!("generations     : {}", sol.stats.generations);
    println!("wall time       : {} ms", wall);
    for w in &sol.warnings {
        println!("warn            : {w}");
    }
}
