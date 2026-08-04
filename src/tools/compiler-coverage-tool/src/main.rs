//! Builds a browsable coverage report for the compiler out of `llvm-cov
//! export` output.
//!
//! Normally run by `./x run compiler-coverage`. The work comes in two halves:
//! `transform` merges the llvm-cov output down to one entry per function, and
//! `generate-html` turns that into the report. `run` does both in one go.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

mod render;
mod transform;

use transform::{CoverageData, FunctionCategory};

#[derive(Parser)]
#[command(about = "Build a browsable coverage report for the Rust compiler")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Parser)]
enum Command {
    /// Transform the coverage data and build the report from it.
    Run {
        /// Output of `llvm-cov export`.
        coverage_json: PathBuf,
        /// Rust checkout to read source from.
        src_root: PathBuf,
        /// Where to write the report. The other pages go beside it.
        output_html: PathBuf,
    },
    /// Transform the coverage data and stop there.
    Transform {
        /// Output of `llvm-cov export`.
        coverage_json: PathBuf,
        /// Rust checkout to read source from.
        src_root: PathBuf,
        /// Where to write the transformed data.
        output_json: PathBuf,
    },
    /// Build the report from data `transform` wrote earlier.
    GenerateHtml {
        /// Output of the `transform` subcommand.
        coverage_data: PathBuf,
        /// Where to write the report. The other pages go beside it.
        output_html: PathBuf,
    },
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::Run { coverage_json, src_root, output_html } => {
            let data = read_and_transform(&coverage_json, &src_root)?;
            generate_html(&data, &output_html)?;
        }
        Command::Transform { coverage_json, src_root, output_json } => {
            let data = read_and_transform(&coverage_json, &src_root)?;
            let json = serde_json::to_string(&data).context("failed to serialize coverage data")?;
            write_atomically(&output_json, &json)?;
            println!("written to {}", output_json.display());
        }
        Command::GenerateHtml { coverage_data, output_html } => {
            let text = std::fs::read_to_string(&coverage_data)
                .with_context(|| format!("failed to read {}", coverage_data.display()))?;
            let data: CoverageData =
                serde_json::from_str(&text).context("failed to parse coverage data")?;
            generate_html(&data, &output_html)?;
        }
    }

    Ok(())
}

/// Read the llvm-cov output and merge it down to one entry per function.
fn read_and_transform(coverage_json: &Path, src_root: &Path) -> Result<CoverageData> {
    eprintln!("reading {}...", coverage_json.display());
    let json_text = std::fs::read_to_string(coverage_json)
        .with_context(|| format!("failed to read {}", coverage_json.display()))?;

    eprintln!("parsing JSON...");
    transform::process(&json_text, src_root)
}

/// Write the report pages, the source shards, and the stylesheet and scripts.
fn generate_html(data: &CoverageData, output_html: &Path) -> Result<()> {
    let fully_count = count_in(data, FunctionCategory::FullyCovered);
    let partial_count = count_in(data, FunctionCategory::PartiallyCovered);
    let uncovered_count = count_in(data, FunctionCategory::FullyUncovered);
    let total = data.functions.len();

    eprintln!("fully: {fully_count}, partial: {partial_count}, uncovered: {uncovered_count}");

    let base_name =
        output_html.file_stem().and_then(|s| s.to_str()).unwrap_or("report").to_string();
    let out_dir = output_html.parent().unwrap_or(Path::new(".")).to_path_buf();

    let shard_dir_name = format!("{base_name}_sources");
    let shard_dir = out_dir.join(&shard_dir_name);
    eprintln!("writing source shards to {}...", shard_dir.display());
    render::write_source_shards(&data.functions, &data.sources, &shard_dir)?;

    render::write_static_assets(&out_dir)?;

    let paths = render::report_paths(&base_name);

    let covered_lines_total: usize = data
        .functions
        .iter()
        .map(|r| r.line_counts.iter().filter(|c| c.map_or(false, |n| n > 0)).count())
        .sum();
    let tracked_lines_total: usize =
        data.functions.iter().map(|r| r.line_counts.iter().filter(|c| c.is_some()).count()).sum();

    let mut pages = vec![(
        paths.index.clone(),
        render::render_index(
            fully_count,
            partial_count,
            uncovered_count,
            covered_lines_total,
            tracked_lines_total,
            &paths,
        )?,
    )];

    for category in [
        FunctionCategory::FullyUncovered,
        FunctionCategory::PartiallyCovered,
        FunctionCategory::FullyCovered,
    ] {
        let functions: Vec<_> = data.functions.iter().filter(|r| r.category == category).collect();
        pages.push((
            paths.for_category(category).to_string(),
            render::render_category_page(&functions, category, &shard_dir_name, &paths)?,
        ));
    }

    for (filename, html) in &pages {
        write_atomically(&out_dir.join(filename), html)?;
    }

    let index_path = out_dir.join(&paths.index);
    println!("written to {}", index_path.display());
    println!("  fully covered:    {} ({:.1}%)", fully_count, pct(fully_count, total));
    println!("  partially:        {} ({:.1}%)", partial_count, pct(partial_count, total));
    println!("  uncovered:        {} ({:.1}%)", uncovered_count, pct(uncovered_count, total));
    println!("  total:            {}", total);

    Ok(())
}

fn count_in(data: &CoverageData, category: FunctionCategory) -> usize {
    data.functions.iter().filter(|r| r.category == category).count()
}

/// Write to a temp file and rename it over the target.
///
/// Crashing halfway through then leaves the previous report in place rather
/// than a half written page.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, contents)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!("failed to rename {} to {}", tmp_path.display(), path.display())
    })?;
    Ok(())
}

pub(crate) fn pct(n: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { n as f64 / total as f64 * 100.0 }
}
