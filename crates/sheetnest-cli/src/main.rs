//! Command-line nesting for sheet cutting.
//!
//! Three subcommands: `nest` packs drawings onto stock, `validate` measures a
//! finished layout, `bench` times the nester over a folder of parts.

mod bench;
mod nest;
mod opts;
mod validate;

use clap::{Parser, Subcommand};

/// Pack parts onto sheet stock with as little waste as possible.
///
/// Reads DXF drawings, works out where each part goes, and writes a cutting
/// file for the laser, plasma or router. All lengths are millimetres, all
/// angles degrees.
#[derive(Parser, Debug)]
#[command(name = "sheetnest", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Pack drawings onto sheets and write the cutting file
    ///
    /// Name each drawing once, with the number of copies after a colon:
    ///
    ///     sheetnest nest bracket.dxf:12 gusset.dxf:4 -o job.dxf
    ///
    /// Parts that will not fit anywhere are reported and left out; the run
    /// still succeeds, so check the "placed" line before cutting.
    #[command(verbatim_doc_comment)]
    Nest(nest::NestArgs),

    /// Check a nested DXF for overlaps, edge clearance and gaps.
    ///
    /// Reads the finished file back the way a cutter would and measures it,
    /// rather than asking the nester whether it did a good job. Exits
    /// non-zero if anything is wrong.
    Validate(validate::ValidateArgs),

    /// Nest a folder of drawings and print the numbers, for comparing runs.
    ///
    /// Every `.dxf` in the folder joins one job. Useful for weighing two
    /// sets of cutting parameters against the same pile of parts.
    Bench(bench::BenchArgs),
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Nest(a) => nest::run(a),
        Command::Validate(a) => validate::run(a),
        Command::Bench(a) => bench::run(a),
    };
    if let Err(err) = result {
        eprintln!("error: {err:#}");
        std::process::exit(2);
    }
}
