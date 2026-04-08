// coverage-tool: collects compiler coverage by running UI tests through an
// instrumented stage1 rustc and merging the resulting profraw files eagerly.
//
// Usage:
//   coverage-tool --config <compiletest-args> [--suite tests/ui/generics] [--out coverage/]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::{env, fs};

use compiletest::{collect_and_make_tests, parse_config};
use compiletest::common::Config;

fn main() {
    let args: Vec<String> = env::args().collect();

    // Minimal CLI: --suite <dir> --out <dir>
    // Everything else is passed through to compiletest's config parser.
    let suite = arg_value(&args, "--suite")
        .unwrap_or_else(|| "tests/ui".to_string());
    let out_dir = arg_value(&args, "--out")
        .unwrap_or_else(|| "coverage".to_string());

    fs::create_dir_all(&out_dir).expect("failed to create output dir");

    // Build a minimal compiletest Config pointing at the test suite.
    // In practice this would be constructed from bootstrap's config,
    // but for now we parse the same flags compiletest accepts.
    let config = build_config(&suite);
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
        let _result = Command::new(config.rustc_path.as_str())
            .arg("--sysroot")
            .arg(config.sysroot_base.as_str())
            .arg(test_file)
            .arg("--edition")
            .arg(test.revision.as_deref().unwrap_or("2015"))
            .args(&["-o", "/dev/null", "--crate-type", "bin"])
            .env("LLVM_PROFILE_FILE", &profile_file)
            .output();

        // Collect any profraw files written
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

        // Eager merge into running profdata, then delete profraws
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
    // Look in the same place bootstrap puts it
    glob::glob("build/*/ci-llvm/bin/llvm-profdata")
        .unwrap()
        .filter_map(|p| p.ok())
        .next()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "llvm-profdata".to_string())
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

fn build_config(suite: &str) -> Config {
    // Construct a minimal Config from environment/args.
    // This is a stub — in practice bootstrap would pass the full config.
    parse_config(vec![
        "coverage-tool".to_string(),
        "--mode".to_string(), "ui".to_string(),
        "--suite-path".to_string(), suite.to_string(),
        "--rustc-path".to_string(), find_stage1_rustc(),
    ])
}

fn find_stage1_rustc() -> String {
    glob::glob("build/*/stage1/bin/rustc")
        .unwrap()
        .filter_map(|p| p.ok())
        .next()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "rustc".to_string())
}
