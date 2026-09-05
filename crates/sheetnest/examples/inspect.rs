//! Inspect DXF files with the engine's parser: parts, ring sizes, and the
//! effect of NFP simplification.
//! Usage: cargo run --release --example inspect -- <dir-or-file> ...

use sheetnest::{dxf, geom};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for a in &args {
        let p = std::path::Path::new(a);
        if p.is_dir() {
            let mut v: Vec<_> = std::fs::read_dir(p)
                .expect("read_dir")
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dxf")))
                .collect();
            v.sort();
            files.extend(v);
        } else {
            files.push(p.to_path_buf());
        }
    }
    if files.is_empty() {
        eprintln!("usage: inspect <dir-or-file.dxf> ...");
        std::process::exit(2);
    }

    for f in files {
        let bytes = std::fs::read(&f).expect("read file");
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        match dxf::parse_dxf(&bytes, &name, 1, 0.25) {
            Ok(parsed) => {
                for part in &parsed.parts {
                    let raw = &part.outer_poly;
                    let s025 = geom::simplify_ring(raw, 0.25);
                    let s1 = geom::simplify_ring(raw, 1.0);
                    println!(
                        "{:<28} part={:<12} holes={} outer_pts={:>5} simp0.25={:>4} simp1.0={:>4} area={:>10.1} bbox_pts_perim={:.1}",
                        name,
                        part.name,
                        part.holes.len(),
                        raw.len(),
                        s025.len(),
                        s1.len(),
                        part.gross_area,
                        part.outer.perimeter(),
                    );
                }
                for w in &parsed.warnings {
                    println!("{name:<28} WARN: {w}");
                }
            }
            Err(e) => println!("{name:<28} ERROR: {e}"),
        }
    }
}
