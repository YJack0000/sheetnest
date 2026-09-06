//! End-to-end tests: run the real binary the way an operator would.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_sheetnest");

/// Keep runs short and repeatable: a fixed seed plus an early give-up.
const FAST: [&str; 6] = [
    "--seed",
    "42",
    "--stale-generations",
    "30",
    "--time-limit-ms",
    "20000",
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .canonicalize()
        .expect("fixtures directory")
}

fn fixture(name: &str) -> String {
    fixtures().join(name).to_string_lossy().into_owned()
}

/// A scratch directory that cleans up after itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sheetnest-cli-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch(dir)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running {BIN} {args:?}: {e}"))
}

fn run_ok(args: &[&str]) -> String {
    let out = run(args);
    assert!(
        out.status.success(),
        "`sheetnest {}` failed ({}):\n{}\n{}",
        args.join(" "),
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn help_works_for_every_subcommand() {
    let top = run_ok(&["--help"]);
    for sub in ["nest", "validate", "bench"] {
        assert!(
            top.contains(sub),
            "top-level help should list `{sub}`:\n{top}"
        );
        let help = run_ok(&[sub, "--help"]);
        assert!(
            help.contains("Usage:"),
            "`{sub} --help` should show usage:\n{help}"
        );
    }
    assert!(run_ok(&["nest", "--help"]).contains("--sheet"));
    assert!(run_ok(&["validate", "--help"]).contains("--spacing"));
    assert!(run_ok(&["bench", "--help"]).contains("--time-limit-ms"));
}

#[test]
fn nest_writes_a_dxf_that_validates() {
    let scratch = Scratch::new("nest");
    let out = scratch.path("job.dxf");
    let svg = scratch.path("job.svg");

    let mut args: Vec<String> = vec![
        "nest".into(),
        format!("{}:2", fixture("bracket_l.dxf")),
        format!("{}:2", fixture("disc.dxf")),
        fixture("gusset.dxf"),
        fixture("plate_rounded.dxf"),
        fixture("strip.dxf"),
        "-o".into(),
        out.clone(),
        "--svg".into(),
        svg.clone(),
        "--sheet".into(),
        "1829x914".into(),
        "--spacing".into(),
        "2".into(),
        "--margin".into(),
        "5".into(),
    ];
    args.extend(FAST.iter().map(|s| s.to_string()));
    let summary = run_ok(&args.iter().map(String::as_str).collect::<Vec<_>>());

    assert!(summary.contains("placed          : 7/7"), "{summary}");
    assert!(Path::new(&out).is_file(), "no DXF written to {out}");
    assert!(Path::new(&svg).is_file(), "no SVG written to {svg}");
    assert!(std::fs::metadata(&out).unwrap().len() > 0);

    let report = run_ok(&[
        "validate",
        &out,
        "--expect",
        "7",
        "--spacing",
        "2",
        "--margin",
        "5",
        "--sheet",
        "1829x914",
    ]);
    assert!(report.contains("VALIDATION PASSED"), "{report}");
}

#[test]
fn json_output_parses_and_places_everything() {
    let disc = format!("{}:4", fixture("disc.dxf"));
    let mut args: Vec<&str> = vec!["nest", &disc, "--json"];
    args.extend(FAST);
    let stdout = run_ok(&args);

    let value: serde_json::Value = serde_json::from_str(&stdout).expect("JSON on stdout");
    let stats = &value["stats"];
    assert_eq!(stats["placed"], stats["total"], "{stdout}");
    assert_eq!(stats["placed"], 4, "{stdout}");
    assert_eq!(value["placements"].as_array().map(Vec::len), Some(4));
    assert!(value["sheetWidth"].is_number(), "{stdout}");
}

#[test]
fn repeated_drawings_get_distinct_part_names() {
    let disc = fixture("disc.dxf");
    let mut args: Vec<&str> = vec!["nest", &disc, &disc, "--json"];
    args.extend(FAST);
    let value: serde_json::Value = serde_json::from_str(&run_ok(&args)).expect("JSON on stdout");

    let mut names: Vec<String> = value["placements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["partName"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(
        names,
        vec!["disc.dxf".to_string(), "disc.dxf#2".to_string()]
    );
}

#[test]
fn bench_reports_the_whole_folder() {
    let dir = fixtures().to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["bench", &dir, "2"];
    args.extend(FAST);
    let stdout = run_ok(&args);
    assert!(stdout.contains("10 instances"), "{stdout}");
    assert!(stdout.contains("placed          : 10/10"), "{stdout}");
    assert!(stdout.contains("strip util"), "{stdout}");
}

#[test]
fn quiet_says_nothing_on_stdout() {
    let scratch = Scratch::new("quiet");
    let out = scratch.path("quiet.dxf");
    let disc = fixture("disc.dxf");
    let mut args: Vec<&str> = vec!["nest", &disc, "-o", &out, "--quiet"];
    args.extend(FAST);
    assert_eq!(run_ok(&args), "");
}

#[test]
fn bad_input_fails_loudly() {
    // A missing drawing.
    let missing = run(&["nest", "no-such-file.dxf", "-o", "x.dxf"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no-such-file.dxf"));

    // A DXF that is not a nesting result has no sheet outlines to check.
    let plain = fixture("disc.dxf");
    let not_a_layout = run(&["validate", &plain]);
    assert!(!not_a_layout.status.success());

    // Nowhere to put the answer.
    let nowhere = run(&["nest", &plain]);
    assert!(!nowhere.status.success());
    assert!(String::from_utf8_lossy(&nowhere.stderr).contains("nothing to write"));
}
