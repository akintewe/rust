// coverage-tool: collects compiler coverage by running UI tests through an
// instrumented stage1 rustc and merging the resulting profraw files eagerly.
//
// Usage:
//   coverage-tool --out coverage/ [compiletest args...]
//
// All flags except --out are forwarded to compiletest's config parser,
// so this tool is intended to be invoked by bootstrap the same way
// compiletest is, with --out added.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::{env, fs};

use compiletest::{collect_and_make_tests, parse_config};

fn main() {
    let args: Vec<String> = env::args().collect();

    // Extract tool-specific flags; everything else goes to parse_config.
    let out_dir = arg_value(&args, "--out")
        .unwrap_or_else(|| "coverage".to_string());

    // Read rustc-path and sysroot-base from args so we can invoke rustc
    // directly without touching private Config fields.
    let rustc_path = arg_value(&args, "--rustc-path")
        .unwrap_or_else(find_stage1_rustc);
    let sysroot = arg_value(&args, "--sysroot-base")
        .unwrap_or_else(find_stage1_sysroot);

    fs::create_dir_all(&out_dir).expect("failed to create output dir");

    // Strip --out <val> from args and pass the rest to parse_config.
    let config_args = strip_flag(args, "--out");
    let config = parse_config(config_args);
    let config = Arc::new(config);

    // Use compiletest to collect the full test list with all directives resolved.
    let tests = collect_and_make_tests(Arc::clone(&config));

    let total = tests.len();
    let mut merged = 0usize;
    let mut skipped = 0usize;

    let profdata_path = PathBuf::from(&out_dir).join("combined.profdata");
    let tmpdir = tempdir();

    for (i, test) in tests.iter().enumerate() {
        if test.desc.ignore {
            skipped += 1;
            continue;
        }

        let test_file = test.testpaths.file.as_str();
        eprint!("[{}/{}] {}  ", i + 1, total, test_file);

        let profile_file = format!("{}/test_%p.profraw", tmpdir.display());

        // Compile the test with LLVM_PROFILE_FILE set so the instrumented
        // rustc emits a profraw file.
        let _result = Command::new(&rustc_path)
            .arg("--sysroot")
            .arg(&sysroot)
            .arg(test_file)
            .args(&["-o", "/dev/null"])
            .env("LLVM_PROFILE_FILE", &profile_file)
            .output();

        // Collect any profraw files written.
        let profraws: Vec<PathBuf> = fs::read_dir(&tmpdir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "profraw").unwrap_or(false))
            .collect();

        if profraws.is_empty() {
            skipped += 1;
            eprintln!("skip");
            continue;
        }

        // Eager merge into running profdata, then delete profraws.
        merge_profraws(&profraws, &profdata_path);
        for f in &profraws {
            let _ = fs::remove_file(f);
        }

        merged += 1;
        eprintln!("ok");
    }

    eprintln!("\nDone. {merged} merged, {skipped} skipped.");
    eprintln!("Profdata: {}", profdata_path.display());
}

fn merge_profraws(profraws: &[PathBuf], profdata: &PathBuf) {
    let llvm_profdata = find_llvm_profdata();
    let mut cmd = Command::new(&llvm_profdata);
    cmd.arg("merge").arg("--sparse").arg("-o").arg(profdata);
    if profdata.exists() {
        cmd.arg(profdata);
    }
    for p in profraws {
        cmd.arg(p);
    }
    let _ = cmd.output();
}

fn find_llvm_profdata() -> String {
    glob::glob("build/*/ci-llvm/bin/llvm-profdata")
        .unwrap()
        .filter_map(|p| p.ok())
        .next()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "llvm-profdata".to_string())
}

fn find_stage1_rustc() -> String {
    glob::glob("build/*/stage1/bin/rustc")
        .unwrap()
        .filter_map(|p| p.ok())
        .next()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "rustc".to_string())
}

fn find_stage1_sysroot() -> String {
    glob::glob("build/*/stage1")
        .unwrap()
        .filter_map(|p| p.ok())
        .next()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "build/host/stage1".to_string())
}

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join("cov_profraws");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}

/// Remove --flag <value> from an args list and return the rest.
fn strip_flag(args: Vec<String>, flag: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == flag {
            skip = true;
            continue;
        }
        out.push(arg);
    }
    out
}
