use std::collections::HashMap;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use rustc_demangle::demangle;
use serde::Deserialize;

// llvm-cov export --format=text JSON structures
#[derive(Deserialize)]
struct Export {
    data: Vec<ExportData>,
}

#[derive(Deserialize)]
struct ExportData {
    functions: Vec<Function>,
}

#[derive(Deserialize)]
struct Function {
    name: String,
    filenames: Vec<String>,
    // regions: [line_start, col_start, line_end, col_end, count, ...]
    regions: Vec<Vec<serde_json::Value>>,
}

#[derive(Clone, Copy, PartialEq)]
enum LineStatus {
    Covered,
    Uncovered,
    Ignored, // not tracked by LLVM (e.g. closing braces)
}

struct FunctionReport {
    demangled: String,
    filename: String,
    line_start: usize,
    line_end: usize,
    // per-line status for lines line_start..=line_end
    lines: Vec<LineStatus>,
}

#[derive(Clone, Copy, PartialEq)]
enum FunctionCategory {
    FullyCovered,
    PartiallyCovered,
    FullyUncovered,
}

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

    eprintln!("reading {}...", json_path.display());
    let json_text = std::fs::read_to_string(&json_path)
        .with_context(|| format!("failed to read {}", json_path.display()))?;

    eprintln!("parsing JSON...");
    let export: Export = serde_json::from_str(&json_text)
        .context("failed to parse llvm-cov JSON")?;

    let functions = export.data.into_iter().flat_map(|d| d.functions).collect::<Vec<_>>();
    eprintln!("{} functions loaded", functions.len());

    // source file cache
    let mut source_cache: HashMap<String, Vec<String>> = HashMap::new();

    let mut reports: Vec<FunctionReport> = vec![];

    for func in &functions {
        let demangled = format!("{:#}", demangle(&func.name));

        // only compiler crates
        if !demangled.starts_with("rustc") {
            continue;
        }

        if func.filenames.is_empty() || func.regions.is_empty() {
            continue;
        }

        // find the primary source file (first one in the compiler/ tree)
        let filename = match func.filenames.iter().find(|f| f.contains("/compiler/")) {
            Some(f) => f.clone(),
            None => func.filenames[0].clone(),
        };

        // figure out overall line span from all regions
        let mut line_start = usize::MAX;
        let mut line_end = 0usize;
        for region in &func.regions {
            if region.len() < 5 { continue; }
            let rs = region[0].as_u64().unwrap_or(0) as usize;
            let re = region[2].as_u64().unwrap_or(0) as usize;
            if rs > 0 && rs < line_start { line_start = rs; }
            if re > line_end { line_end = re; }
        }
        if line_start == usize::MAX || line_end == 0 || line_start > line_end {
            continue;
        }

        // build per-line coverage map from regions
        // region format: [line_start, col_start, line_end, col_end, count, file_id, ...]
        let mut line_counts: HashMap<usize, u64> = HashMap::new();
        for region in &func.regions {
            if region.len() < 5 { continue; }
            let rs = region[0].as_u64().unwrap_or(0) as usize;
            let re = region[2].as_u64().unwrap_or(0) as usize;
            let count = region[4].as_u64().unwrap_or(0);
            for line in rs..=re {
                // keep the minimum — if any region covering this line is 0, it's uncovered
                let entry = line_counts.entry(line).or_insert(count);
                if count < *entry {
                    *entry = count;
                }
            }
        }

        // load source file
        if !source_cache.contains_key(&filename) {
            // try to resolve path relative to src_root by stripping the absolute prefix
            let resolved = resolve_source_path(&filename, &src_root);
            let lines = match resolved.and_then(|p| std::fs::read_to_string(&p).ok()) {
                Some(text) => text.lines().map(|l| l.to_string()).collect::<Vec<_>>(),
                None => vec![],
            };
            source_cache.insert(filename.clone(), lines);
        }

        let source_lines = &source_cache[&filename];

        let mut lines = vec![];
        for lineno in line_start..=line_end {
            let status = match line_counts.get(&lineno) {
                None => LineStatus::Ignored,
                Some(0) => LineStatus::Uncovered,
                Some(_) => LineStatus::Covered,
            };
            // if source is empty we still emit the status
            let _ = source_lines.get(lineno.saturating_sub(1));
            lines.push(status);
        }

        reports.push(FunctionReport {
            demangled,
            filename,
            line_start,
            line_end,
            lines,
        });
    }

    eprintln!("{} compiler functions processed", reports.len());

    // categorise
    let categorised: Vec<(&FunctionReport, FunctionCategory)> = reports.iter().map(|r| {
        let tracked: Vec<_> = r.lines.iter().filter(|&&s| s != LineStatus::Ignored).collect();
        let cat = if tracked.is_empty() {
            FunctionCategory::FullyCovered
        } else if tracked.iter().all(|&&s| s == LineStatus::Covered) {
            FunctionCategory::FullyCovered
        } else if tracked.iter().all(|&&s| s == LineStatus::Uncovered) {
            FunctionCategory::FullyUncovered
        } else {
            FunctionCategory::PartiallyCovered
        };
        (r, cat)
    }).collect();

    let fully_count = categorised.iter().filter(|(_, c)| *c == FunctionCategory::FullyCovered).count();
    let partial_count = categorised.iter().filter(|(_, c)| *c == FunctionCategory::PartiallyCovered).count();
    let uncovered_count = categorised.iter().filter(|(_, c)| *c == FunctionCategory::FullyUncovered).count();
    let total = fully_count + partial_count + uncovered_count;

    eprintln!("fully: {fully_count}, partial: {partial_count}, uncovered: {uncovered_count}");

    let html = render_html(&categorised, &source_cache, fully_count, partial_count, uncovered_count, total);

    std::fs::write(&output_path, html)
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    println!("written to {}", output_path.display());
    println!("  fully covered:    {} ({:.1}%)", fully_count, pct(fully_count, total));
    println!("  partially:        {} ({:.1}%)", partial_count, pct(partial_count, total));
    println!("  uncovered:        {} ({:.1}%)", uncovered_count, pct(uncovered_count, total));
    println!("  total:            {}", total);

    Ok(())
}

fn resolve_source_path(filename: &str, src_root: &Path) -> Option<PathBuf> {
    // filename is an absolute path like /home/gh-akintewe/rust/compiler/rustc_ast/src/...
    // we want to find /compiler/... and join it with src_root
    if let Some(idx) = filename.find("/compiler/") {
        let rel = &filename[idx + 1..]; // "compiler/rustc_ast/src/..."
        let candidate = src_root.join(rel);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // fallback: try the path as-is
    let p = PathBuf::from(filename);
    if p.exists() { Some(p) } else { None }
}

fn render_html(
    categorised: &[(&FunctionReport, FunctionCategory)],
    source_cache: &HashMap<String, Vec<String>>,
    fully_count: usize,
    partial_count: usize,
    uncovered_count: usize,
    total: usize,
) -> String {
    let mut out = String::new();

    out.push_str(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Compiler Coverage Report</title>
<style>
body { font-family: monospace; margin: 0; background: #1e1e1e; color: #d4d4d4; }
h1 { color: #9cdcfe; padding: 1em 2em 0; }
.summary { display: flex; gap: 1.5em; padding: 1em 2em 2em; }
.box { padding: 1em 2em; border-radius: 4px; text-align: center; min-width: 10em; }
.box .num { font-size: 2em; font-weight: bold; }
.box .label { font-size: 0.85em; color: #aaa; margin-top: 0.3em; }
.green-box { background: #1a2e1a; border: 1px solid #4caf50; }
.yellow-box { background: #2e2a1a; border: 1px solid #ffeb3b; }
.red-box { background: #2e1a1a; border: 1px solid #f44336; }
.filters { padding: 0 2em 1em; display: flex; gap: 0.5em; }
.filters button { padding: 0.4em 1em; border-radius: 3px; border: none; cursor: pointer; font-family: monospace; }
.btn-partial { background: #3a3a1e; color: #ffeb3b; }
.btn-uncovered { background: #3a1e1e; color: #f44336; }
.btn-fully { background: #1e3a1e; color: #4caf50; }
.btn-all { background: #2a2a2a; color: #d4d4d4; }
.section { padding: 0 2em; }
.fn-block { margin-bottom: 0.5em; border-radius: 3px; overflow: hidden; }
.fn-block.hidden { display: none; }
details > summary {
  padding: 0.5em 1em;
  cursor: pointer;
  list-style: none;
  display: flex;
  justify-content: space-between;
  align-items: center;
  user-select: none;
}
details > summary::-webkit-details-marker { display: none; }
.hdr-fully { background: #1a2e1a; }
.hdr-partial { background: #2e2a1a; }
.hdr-uncovered { background: #2e1a1a; }
.fn-name { font-size: 0.9em; word-break: break-all; }
.fn-meta { font-size: 0.8em; color: #888; white-space: nowrap; margin-left: 1em; }
.source { background: #141414; overflow-x: auto; }
table.src { border-collapse: collapse; width: 100%; }
td.lineno { color: #555; text-align: right; padding: 0 0.8em; min-width: 3em; user-select: none; border-right: 1px solid #333; }
td.code { padding: 0 1em; white-space: pre; }
tr.line-covered td.code { background: #1a2e1a; }
tr.line-uncovered td.code { background: #2e1a1a; }
tr.line-ignored td.code { }
</style>
</head>
<body>
"#);

    out.push_str(&format!(
        r#"<h1>Compiler Coverage Report</h1>
<div class="summary">
  <div class="box green-box">
    <div class="num">{}</div>
    <div class="label">Fully Covered<br>{:.1}%</div>
  </div>
  <div class="box yellow-box">
    <div class="num">{}</div>
    <div class="label">Partially Covered<br>{:.1}%</div>
  </div>
  <div class="box red-box">
    <div class="num">{}</div>
    <div class="label">Uncovered<br>{:.1}%</div>
  </div>
</div>
"#,
        fully_count, pct(fully_count, total),
        partial_count, pct(partial_count, total),
        uncovered_count, pct(uncovered_count, total),
    ));

    out.push_str(r#"<div class="filters">
  <button class="btn-all" onclick="show('all')">All</button>
  <button class="btn-uncovered" onclick="show('uncovered')">Uncovered only</button>
  <button class="btn-partial" onclick="show('partial')">Partially covered only</button>
  <button class="btn-fully" onclick="show('fully')">Fully covered only</button>
</div>
<script>
function show(cat) {
  document.querySelectorAll('.fn-block').forEach(el => {
    if (cat === 'all') { el.classList.remove('hidden'); }
    else { el.classList.toggle('hidden', !el.classList.contains('cat-' + cat)); }
  });
}
</script>
<div class="section">
"#);

    for (report, cat) in categorised {
        let (hdr_class, cat_class) = match cat {
            FunctionCategory::FullyCovered => ("hdr-fully", "cat-fully"),
            FunctionCategory::PartiallyCovered => ("hdr-partial", "cat-partial"),
            FunctionCategory::FullyUncovered => ("hdr-uncovered", "cat-uncovered"),
        };

        let short_filename = if let Some(idx) = report.filename.find("/compiler/") {
            &report.filename[idx + 1..]
        } else {
            &report.filename
        };

        let covered_lines = report.lines.iter().filter(|&&s| s == LineStatus::Covered).count();
        let total_tracked = report.lines.iter().filter(|&&s| s != LineStatus::Ignored).count();
        let pct_str = if total_tracked > 0 {
            format!("{:.0}%", pct(covered_lines, total_tracked))
        } else {
            "100%".to_string()
        };

        out.push_str(&format!(
            r#"<div class="fn-block {cat_class}">
<details>
<summary class="{hdr_class}">
  <span class="fn-name">{fn_name}</span>
  <span class="fn-meta">{pct_str} &nbsp;|&nbsp; {short_file}:{line_start}</span>
</summary>
<div class="source"><table class="src">
"#,
            cat_class = cat_class,
            hdr_class = hdr_class,
            fn_name = escape(&report.demangled),
            pct_str = pct_str,
            short_file = escape(short_filename),
            line_start = report.line_start,
        ));

        let source_lines = source_cache.get(&report.filename);

        for (i, &status) in report.lines.iter().enumerate() {
            let lineno = report.line_start + i;
            let line_class = match status {
                LineStatus::Covered => "line-covered",
                LineStatus::Uncovered => "line-uncovered",
                LineStatus::Ignored => "line-ignored",
            };
            let src_text = source_lines
                .and_then(|ls| ls.get(lineno.saturating_sub(1)))
                .map(|s| s.as_str())
                .unwrap_or("");

            out.push_str(&format!(
                "<tr class=\"{line_class}\"><td class=\"lineno\">{lineno}</td><td class=\"code\">{code}</td></tr>\n",
                line_class = line_class,
                lineno = lineno,
                code = escape(src_text),
            ));
        }

        out.push_str("</table></div></details></div>\n");
    }

    out.push_str("</div></body></html>\n");
    out
}

fn pct(n: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { n as f64 / total as f64 * 100.0 }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
