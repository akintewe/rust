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

struct FunctionReport {
    demangled: String,
    filename: String,
    line_start: usize,
    line_end: usize,
    // per-line hit counts for lines line_start..=line_end
    // None = LLVM doesn't track this line (closing braces etc)
    // Some(n) = total hits across all merged monomorphizations
    line_counts: Vec<Option<u64>>,
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

        // only compiler crates — match `rustc_foo::` at the start or after a leading `<`
        if !demangled.starts_with("rustc") && !demangled.contains("<rustc") {
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

        // build per-line hit count map from regions
        // region format: [line_start, col_start, line_end, col_end, count, file_id, expanded_file_id, kind]
        // kind: 0=code, 1=expansion, 2=skipped, 3=gap — only count kind=0 (real code regions)
        //
        // For each line, use the innermost (tightest-enclosing) region's count.
        // LLVM emits nested regions for branches — outer has the function/block count,
        // inner has the branch-taken count. The innermost region (latest line_start,
        // earliest line_end) is the most specific counter for what actually ran on
        // that line. Taking the minimum instead incorrectly attributes outer counts
        // to lines only reached by specific branches.
        //
        // Tightness = smallest line span; ties broken by latest line_start (higher rs).
        // key: line → (span_lines, Reverse(rs), count)
        let mut region_tightness: HashMap<usize, (usize, std::cmp::Reverse<usize>, u64)> = HashMap::new();
        for region in &func.regions {
            if region.len() < 8 { continue; }
            let kind = region[7].as_u64().unwrap_or(0);
            if kind != 0 { continue; }
            let rs = region[0].as_u64().unwrap_or(0) as usize;
            let re = region[2].as_u64().unwrap_or(0) as usize;
            let count = region[4].as_u64().unwrap_or(0);
            let span = re.saturating_sub(rs);
            for line in rs..=re {
                let key = (span, std::cmp::Reverse(rs), count);
                let entry = region_tightness.entry(line).or_insert(key);
                // prefer tighter (smaller span, then later start)
                if (span, std::cmp::Reverse(rs)) < (entry.0, entry.1) {
                    *entry = key;
                }
            }
        }
        let region_counts: HashMap<usize, u64> = region_tightness
            .into_iter()
            .map(|(line, (_, _, count))| (line, count))
            .collect();

        // load source file
        if !source_cache.contains_key(&filename) {
            let resolved = resolve_source_path(&filename, &src_root);
            let lines = match resolved.and_then(|p| std::fs::read_to_string(&p).ok()) {
                Some(text) => text.lines().map(|l| l.to_string()).collect::<Vec<_>>(),
                None => vec![],
            };
            source_cache.insert(filename.clone(), lines);
        }

        // store raw counts — None means LLVM doesn't track this line
        let line_counts: Vec<Option<u64>> = (line_start..=line_end)
            .map(|lineno| region_counts.get(&lineno).copied())
            .collect();

        reports.push(FunctionReport {
            demangled,
            filename,
            line_start,
            line_end,
            line_counts,
        });
    }

    eprintln!("{} compiler functions processed (before merging monomorphizations)", reports.len());

    // merge monomorphizations — group by (filename, line_start), union line coverage
    // if any instantiation covered a line, that line counts as covered
    let reports = merge_monomorphizations(reports);

    eprintln!("{} functions after merging monomorphizations", reports.len());

    // categorise based on summed counts
    let categorised: Vec<(&FunctionReport, FunctionCategory)> = reports.iter().map(|r| {
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
        (r, cat)
    }).collect();

    let fully_count = categorised.iter().filter(|(_, c)| *c == FunctionCategory::FullyCovered).count();
    let partial_count = categorised.iter().filter(|(_, c)| *c == FunctionCategory::PartiallyCovered).count();
    let uncovered_count = categorised.iter().filter(|(_, c)| *c == FunctionCategory::FullyUncovered).count();
    let total = fully_count + partial_count + uncovered_count;

    eprintln!("fully: {fully_count}, partial: {partial_count}, uncovered: {uncovered_count}");

    let html = render_html(&categorised, &source_cache, fully_count, partial_count, uncovered_count, total);

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

fn merge_monomorphizations(reports: Vec<FunctionReport>) -> Vec<FunctionReport> {
    // key: (filename, line_start) — same source location = same generic function
    // sum hit counts across all monomorphizations — a branch covered by one mono
    // contributes its count to the total, so two partials can become fully covered
    let mut groups: std::collections::BTreeMap<(String, usize), FunctionReport> = std::collections::BTreeMap::new();

    for report in reports {
        let key = (report.filename.clone(), report.line_start);
        match groups.get_mut(&key) {
            None => { groups.insert(key, report); }
            Some(existing) => {
                // sum counts — None (ignored) stays None, Some values are added
                for (i, count) in report.line_counts.iter().enumerate() {
                    if let Some(existing_count) = existing.line_counts.get_mut(i) {
                        *existing_count = match (*existing_count, *count) {
                            (Some(a), Some(b)) => Some(a.saturating_add(b)),
                            (Some(a), None) => Some(a),
                            (None, Some(b)) => Some(b),
                            (None, None) => None,
                        };
                    }
                }
                // prefer shorter name — less generic type noise
                if report.demangled.len() < existing.demangled.len() {
                    existing.demangled = report.demangled;
                }
            }
        }
    }

    groups.into_values().collect()
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
    let covered_lines_total: usize = categorised.iter().map(|(r, _)| {
        r.line_counts.iter().filter(|c| c.map_or(false, |n| n > 0)).count()
    }).sum();
    let tracked_lines_total: usize = categorised.iter().map(|(r, _)| {
        r.line_counts.iter().filter(|c| c.is_some()).count()
    }).sum();
    let overall_pct = pct(covered_lines_total, tracked_lines_total);

    let mut out = String::new();

    out.push_str(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Compiler Coverage Report</title>
<style>
* { box-sizing: border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 0; background: #f8f9fa; color: #212529; }
.header { background: #fff; border-bottom: 1px solid #dee2e6; padding: 1.5em 2em; }
.header h1 { margin: 0 0 0.2em; font-size: 1.4em; color: #343a40; }
.overall { font-size: 2.2em; font-weight: bold; color: #212529; margin: 0.2em 0; }
.overall-sub { color: #6c757d; font-size: 0.95em; }
.filter-bar { display: flex; gap: 0.5em; padding: 1em 2em; background: #fff; border-bottom: 1px solid #dee2e6; flex-wrap: wrap; align-items: center; }
.filter-bar span { color: #6c757d; font-size: 0.9em; margin-right: 0.5em; }
.filter-bar button {
  padding: 0.35em 1em; border-radius: 20px; border: 1px solid; cursor: pointer;
  font-size: 0.85em; font-family: inherit; background: #fff;
}
.search-bar { padding: 0.6em 2em; background: #fff; border-bottom: 1px solid #dee2e6; display: flex; align-items: center; gap: 0.5em; }
.search-bar input {
  padding: 0.4em 0.8em; border: 1px solid #ced4da; border-radius: 4px;
  font-size: 0.9em; font-family: "SFMono-Regular", Consolas, monospace;
  width: 30em; outline: none;
}
.search-bar input:focus { border-color: #86b7fe; box-shadow: 0 0 0 2px rgba(13,110,253,0.15); }
.search-bar .search-count { font-size: 0.85em; color: #6c757d; }
.btn-all { border-color: #adb5bd; color: #495057; }
.btn-all.active, .btn-all:hover { background: #495057; color: #fff; }
.btn-uncovered { border-color: #dc3545; color: #dc3545; }
.btn-uncovered.active, .btn-uncovered:hover { background: #dc3545; color: #fff; }
.btn-partial { border-color: #fd7e14; color: #fd7e14; }
.btn-partial.active, .btn-partial:hover { background: #fd7e14; color: #fff; }
.btn-fully { border-color: #198754; color: #198754; }
.btn-fully.active, .btn-fully:hover { background: #198754; color: #fff; }
.section-header {
  padding: 0.6em 2em; font-size: 0.85em; font-weight: 600; text-transform: uppercase;
  letter-spacing: 0.05em; color: #fff; margin-top: 1px;
}
.section-header.uncovered { background: #dc3545; }
.section-header.partial { background: #fd7e14; }
.section-header.fully { background: #198754; }
.fn-list { padding: 0 2em; }
.fn-block { border: 1px solid #dee2e6; border-radius: 4px; margin: 0.5em 0; background: #fff; overflow: hidden; }
.fn-block.hidden { display: none; }
details > summary {
  padding: 0.7em 1em; cursor: pointer; list-style: none;
  display: grid; grid-template-columns: 1fr auto;
  align-items: start; gap: 1em; user-select: none;
}
details > summary:hover { background: #f8f9fa; }
details > summary::-webkit-details-marker { display: none; }
.fn-left { min-width: 0; }
.fn-name { font-size: 0.9em; font-weight: 600; color: #212529; word-break: break-all; font-family: "SFMono-Regular", Consolas, monospace; }
.fn-file { font-size: 0.8em; color: #6c757d; margin-top: 0.2em; font-family: "SFMono-Regular", Consolas, monospace; }
.fn-badge {
  font-size: 0.8em; font-weight: 600; padding: 0.2em 0.6em;
  border-radius: 3px; white-space: nowrap; font-family: inherit;
}
.badge-fully { background: #d1e7dd; color: #0a3622; }
.badge-partial { background: #ffe5d0; color: #6c2a00; }
.badge-uncovered { background: #f8d7da; color: #58151c; }
.source-view { border-top: 1px solid #dee2e6; overflow-x: auto; background: #fdfdfd; }
table.src { border-collapse: collapse; width: 100%; font-family: "SFMono-Regular", Consolas, monospace; font-size: 0.82em; }
td.lineno {
  color: #adb5bd; text-align: right; padding: 1px 0.8em; min-width: 3.5em;
  user-select: none; border-right: 2px solid #dee2e6; vertical-align: top;
}
td.code { padding: 1px 1em; white-space: pre; }
tr.line-covered td.lineno { border-right-color: #198754; }
tr.line-covered td.code { background: #d1e7dd; }
tr.line-uncovered td.lineno { border-right-color: #dc3545; }
tr.line-uncovered td.code { background: #f8d7da; }
tr.line-ignored td.code { color: #6c757d; }
</style>
</head>
<body>
"#);

    out.push_str(&format!(
        r#"<div class="header">
  <h1>Rust Compiler Coverage Report</h1>
  <div class="overall">{overall_pct:.2}% ({covered_lines_total}/{tracked_lines_total} lines)</div>
  <div class="overall-sub">Below is a list of all functions in the compiler. Use the expander to review line coverage of any function.</div>
</div>
<div class="filter-bar">
  <span>Filter by status:</span>
  <button class="btn-all active" onclick="show('all', this)">{total} All</button>
  <button class="btn-fully" onclick="show('fully', this)">{fully_count} Fully Covered</button>
  <button class="btn-partial" onclick="show('partial', this)">{partial_count} Partially Covered</button>
  <button class="btn-uncovered" onclick="show('uncovered', this)">{uncovered_count} Fully Uncovered</button>
</div>
<script>
var currentCat = 'all';
var currentSearch = '';

function applyFilters() {{
  var query = currentSearch.toLowerCase();
  var visible = 0;
  document.querySelectorAll('.fn-block').forEach(el => {{
    var catMatch = currentCat === 'all' || el.classList.contains('cat-' + currentCat);
    var name = el.querySelector('.fn-name') ? el.querySelector('.fn-name').textContent.toLowerCase() : '';
    var file = el.querySelector('.fn-file') ? el.querySelector('.fn-file').textContent.toLowerCase() : '';
    var searchMatch = query === '' || name.includes(query) || file.includes(query);
    var hide = !(catMatch && searchMatch);
    el.classList.toggle('hidden', hide);
    if (!hide) visible++;
  }});
  document.querySelectorAll('.section-header').forEach(el => {{
    if (currentCat === 'all') {{ el.style.display = ''; }}
    else {{ el.style.display = el.classList.contains(currentCat) ? '' : 'none'; }}
  }});
  var countEl = document.getElementById('search-count');
  if (countEl) countEl.textContent = query ? visible + ' result' + (visible === 1 ? '' : 's') : '';
}}

function show(cat, btn) {{
  document.querySelectorAll('.filter-bar button').forEach(b => b.classList.remove('active'));
  btn.classList.add('active');
  currentCat = cat;
  applyFilters();
}}

function onSearch(val) {{
  currentSearch = val;
  applyFilters();
}}
</script>
<div class="search-bar">
  <input type="text" placeholder="Search functions or file paths..." oninput="onSearch(this.value)" />
  <span class="search-count" id="search-count"></span>
</div>
"#,
        overall_pct = overall_pct,
        covered_lines_total = covered_lines_total,
        tracked_lines_total = tracked_lines_total,
        total = total,
        fully_count = fully_count,
        partial_count = partial_count,
        uncovered_count = uncovered_count,
    ));

    // render in order: uncovered first (most interesting), then partial, then fully
    let order = [
        (FunctionCategory::FullyUncovered, "uncovered", "Fully Uncovered", uncovered_count),
        (FunctionCategory::PartiallyCovered, "partial", "Partially Covered", partial_count),
        (FunctionCategory::FullyCovered, "fully", "Fully Covered", fully_count),
    ];

    for (cat_variant, cat_class, label, count) in &order {
        out.push_str(&format!(
            "<div class=\"section-header {cat_class}\">{label} ({count})</div>\n<div class=\"fn-list\">\n",
            cat_class = cat_class,
            label = label,
            count = count,
        ));

        for (report, cat) in categorised {
            if cat != cat_variant { continue; }

            let short_filename = if let Some(idx) = report.filename.find("/compiler/") {
                &report.filename[idx + 1..]
            } else {
                &report.filename
            };

            let covered_lines = report.line_counts.iter().filter(|c| c.map_or(false, |n| n > 0)).count();
            let total_tracked = report.line_counts.iter().filter(|c| c.is_some()).count();
            let fn_pct = if total_tracked > 0 { pct(covered_lines, total_tracked) } else { 100.0 };

            let (badge_class, badge_text) = match cat_variant {
                FunctionCategory::FullyCovered => ("badge-fully", format!("100% covered")),
                FunctionCategory::PartiallyCovered => ("badge-partial", format!("{fn_pct:.0}% covered")),
                FunctionCategory::FullyUncovered => ("badge-uncovered", "0% covered".to_string()),
            };

            out.push_str(&format!(
                r#"<div class="fn-block cat-{cat_class}">
<details>
<summary>
  <div class="fn-left">
    <div class="fn-name">{fn_name}</div>
    <div class="fn-file">File: {short_file}:{line_start}</div>
  </div>
  <span class="fn-badge {badge_class}">{badge_text}</span>
</summary>
<div class="source-view"><table class="src">
"#,
                cat_class = cat_class,
                fn_name = escape(&report.demangled),
                short_file = escape(short_filename),
                line_start = report.line_start,
                badge_class = badge_class,
                badge_text = badge_text,
            ));

            let source_lines = source_cache.get(&report.filename);

            for (i, count) in report.line_counts.iter().enumerate() {
                let lineno = report.line_start + i;
                let line_class = match count {
                    Some(n) if *n > 0 => "line-covered",
                    Some(_) => "line-uncovered",
                    None => "line-ignored",
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

        out.push_str("</div>\n");
    }

    out.push_str("</body></html>\n");
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
