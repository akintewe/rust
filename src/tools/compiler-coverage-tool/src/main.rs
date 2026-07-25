use std::path::{Path, PathBuf};
use anyhow::{Context, Result};

mod processing;
mod render;

use processing::FunctionCategory;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!(
            "usage: {} <compiler-coverage.json> <rust-src-root> <output.html>",
            args[0]
        );
        eprintln!("  compiler-coverage.json  — filtered llvm-cov export output");
        eprintln!("  rust-src-root           — path to rust repo root (to read source files)");
        eprintln!("  output.html             — output HTML file");
        std::process::exit(1);
    }

    let json_path = PathBuf::from(&args[1]);
    let src_root = PathBuf::from(&args[2]);
    let output_path = PathBuf::from(&args[3]);

    let github_base = render::github_base_url(&src_root);
    match &github_base {
        Some(url) => eprintln!("linking functions to {url}/..."),
        None => eprintln!("no github origin remote found, report will not link to source"),
    }

    eprintln!("reading {}...", json_path.display());
    let json_text = std::fs::read_to_string(&json_path)
        .with_context(|| format!("failed to read {}", json_path.display()))?;

    eprintln!("parsing JSON...");
    let (reports, source_cache) = processing::process(&json_text, &src_root)?;

    // categorise based on summed counts. index in `reports` doubles as a stable
    // id used to look up this function's source lines in the JSON shards.
    let categorised: Vec<(usize, &processing::FunctionReport, FunctionCategory)> = reports.iter().enumerate().map(|(id, r)| {
        let tracked: Vec<u64> = r.line_counts.iter().filter_map(|c| *c).collect();
        let cat = if tracked.is_empty() {
            FunctionCategory::FullyCovered
        } else if tracked.iter().all(|&c| c > 0) {
            FunctionCategory::FullyCovered
        } else if tracked.iter().all(|&c| c == 0) {
            FunctionCategory::FullyUncovered
        } else {
            FunctionCategory::PartiallyCovered
        };
        (id, r, cat)
    }).collect();

    let fully_count = categorised.iter().filter(|(_, _, c)| *c == FunctionCategory::FullyCovered).count();
    let partial_count = categorised.iter().filter(|(_, _, c)| *c == FunctionCategory::PartiallyCovered).count();
    let uncovered_count = categorised.iter().filter(|(_, _, c)| *c == FunctionCategory::FullyUncovered).count();
    let total = fully_count + partial_count + uncovered_count;

    eprintln!("fully: {fully_count}, partial: {partial_count}, uncovered: {uncovered_count}");

    let shard_dir_name = format!(
        "{}_sources",
        output_path.file_stem().and_then(|s| s.to_str()).unwrap_or("report")
    );
    let shard_dir = output_path.parent().unwrap_or(Path::new(".")).join(&shard_dir_name);
    eprintln!("writing source shards to {}...", shard_dir.display());
    render::write_source_shards(&categorised, &source_cache, &shard_dir)?;

    let html = render::render_html(&categorised, fully_count, partial_count, uncovered_count, total, &shard_dir_name, &github_base);

    // Write to a temp file first, then rename atomically — so a crash mid-run
    // never leaves a partial or stale output file behind.
    let tmp_path = output_path.with_extension("html.tmp");
    std::fs::write(&tmp_path, html)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &output_path)
        .with_context(|| format!("failed to rename {} to {}", tmp_path.display(), output_path.display()))?;

    println!("written to {}", output_path.display());
    println!("  fully covered:    {} ({:.1}%)", fully_count, pct(fully_count, total));
    println!("  partially:        {} ({:.1}%)", partial_count, pct(partial_count, total));
    println!("  uncovered:        {} ({:.1}%)", uncovered_count, pct(uncovered_count, total));
    println!("  total:            {}", total);

    Ok(())
}

fn pct(n: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { n as f64 / total as f64 * 100.0 }
}
