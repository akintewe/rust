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
    _line_end: usize,
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
        // also treat lines containing only bug!/span_bug! as None (ignored) —
        // these are intentionally unreachable and not real coverage gaps
        let source_lines = source_cache.get(&filename).cloned().unwrap_or_default();
        let mut line_counts: Vec<Option<u64>> = (line_start..=line_end)
            .map(|lineno| {
                let src = source_lines.get(lineno.saturating_sub(1)).map(|s| s.trim()).unwrap_or("");
                if src.starts_with("bug!") || src.starts_with("span_bug!") {
                    return None;
                }
                region_counts.get(&lineno).copied()
            })
            .collect();

        // if a closing brace line shows as uncovered but the preceding covered line
        // in this function was covered, promote it — LLVM maps branch-not-taken
        // counters to closing braces, making them red when the body above is green
        let mut last_covered_count: Option<u64> = None;
        for i in 0..line_counts.len() {
            let lineno = line_start + i;
            let src = source_lines.get(lineno.saturating_sub(1)).map(|s| s.trim()).unwrap_or("");
            let is_closing = src == "}" || src == "};" || src == "}," || src == "});" || src == "})";
            match line_counts[i] {
                Some(c) if c > 0 => { last_covered_count = Some(c); }
                Some(0) if is_closing => {
                    if let Some(c) = last_covered_count {
                        line_counts[i] = Some(c);
                    }
                }
                _ => {}
            }
        }

        // TODO: propagate uncovered status to preceding ignored lines (e.g. match
        // arm patterns that are grey but whose body is red). Needs a smarter approach
        // than a simple lookahead — naive propagation marks too many lines as uncovered.

        reports.push(FunctionReport {
            demangled,
            filename,
            line_start,
            _line_end: line_end,
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

fn crate_and_module(filename: &str) -> (String, String) {
    // filename like "compiler/rustc_middle/src/ty/mod.rs"
    // returns ("rustc_middle", "ty::mod")
    let rel = if let Some(idx) = filename.find("/compiler/") {
        &filename[idx + "/compiler/".len()..]
    } else {
        filename
    };
    let parts: Vec<&str> = rel.splitn(3, '/').collect();
    let krate = parts.get(1).unwrap_or(&"").to_string(); // e.g. "rustc_middle"
    let path = parts.get(2).unwrap_or(&"").trim_end_matches(".rs");
    let module = path.replace('/', "::");
    (krate, module)
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
body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 0; background: #f8f9fa; color: #212529; font-size: 14px; }
.header { background: #fff; border-bottom: 1px solid #dee2e6; padding: 1.2em 2em; }
.header h1 { margin: 0 0 0.2em; font-size: 1.3em; color: #343a40; }
.overall { font-size: 2em; font-weight: bold; color: #212529; margin: 0.2em 0; }
.overall-sub { color: #6c757d; font-size: 0.9em; }
.toolbar { display: flex; gap: 0.5em; padding: 0.7em 2em; background: #fff; border-bottom: 1px solid #dee2e6; flex-wrap: wrap; align-items: center; }
.toolbar span { color: #6c757d; font-size: 0.85em; margin-right: 0.3em; }
.toolbar button {
  padding: 0.25em 0.8em; border-radius: 20px; border: 1px solid; cursor: pointer;
  font-size: 0.82em; font-family: inherit; background: #fff;
}
.toolbar input {
  padding: 0.3em 0.7em; border: 1px solid #ced4da; border-radius: 4px;
  font-size: 0.85em; font-family: "SFMono-Regular", Consolas, monospace;
  width: 24em; outline: none; margin-left: auto;
}
.toolbar input:focus { border-color: #86b7fe; }
.search-count { font-size: 0.82em; color: #6c757d; }
.btn-all { border-color: #adb5bd; color: #495057; }
.btn-all.active, .btn-all:hover { background: #495057; color: #fff; }
.btn-uncovered { border-color: #dc3545; color: #dc3545; }
.btn-uncovered.active, .btn-uncovered:hover { background: #dc3545; color: #fff; }
.btn-partial { border-color: #fd7e14; color: #fd7e14; }
.btn-partial.active, .btn-partial:hover { background: #fd7e14; color: #fff; }
.btn-fully { border-color: #198754; color: #198754; }
.btn-fully.active, .btn-fully:hover { background: #198754; color: #fff; }
.crate-block { margin: 0; }
.crate-header {
  padding: 0.5em 2em; background: #e9ecef; border-bottom: 1px solid #dee2e6;
  font-weight: 600; font-size: 0.9em; cursor: pointer; user-select: none;
  display: flex; align-items: center; gap: 0.5em;
}
.crate-header:hover { background: #dee2e6; }
.crate-fns { display: block; }
.crate-fns.collapsed { display: none; }
.module-header {
  padding: 0.3em 2em 0.3em 3em; background: #f8f9fa; border-bottom: 1px solid #f0f0f0;
  font-size: 0.82em; color: #6c757d; font-family: "SFMono-Regular", Consolas, monospace;
  cursor: pointer; user-select: none;
}
.module-header:hover { background: #f0f0f0; }
.module-fns { display: block; }
.module-fns.collapsed { display: none; }
.fn-row {
  display: grid; grid-template-columns: 1.2em 1fr auto;
  align-items: center; padding: 0.18em 2em 0.18em 4em;
  cursor: pointer;
  font-family: "SFMono-Regular", Consolas, monospace; font-size: 0.82em;
}
.fn-row:hover { background: #f0f4ff; }
.fn-row.hidden { display: none; }
.fn-dot { width: 8px; height: 8px; border-radius: 50%; display: inline-block; }
.dot-fully { background: #198754; }
.dot-partial { background: #fd7e14; }
.dot-uncovered { background: #dc3545; }
.fn-name { color: #212529; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; padding: 0 0.5em; }
.fn-pct { font-size: 0.8em; color: #6c757d; white-space: nowrap; }
.fn-pct.pct-fully { color: #198754; }
.fn-pct.pct-partial { color: #fd7e14; }
.fn-pct.pct-uncovered { color: #dc3545; }
.source-view { display: none; background: #fdfdfd; border-bottom: 2px solid #dee2e6; overflow-x: auto; }
.source-view.open { display: block; }
table.src { border-collapse: collapse; width: 100%; font-family: "SFMono-Regular", Consolas, monospace; font-size: 0.8em; }
td.lineno {
  color: #adb5bd; text-align: right; padding: 1px 0.8em; min-width: 3.5em;
  user-select: none; border-right: 2px solid #dee2e6; vertical-align: top;
}
td.code { padding: 1px 1em; white-space: pre; }
tr.line-covered td.lineno { border-right-color: #198754; }
tr.line-covered td.code { background: #d1e7dd; }
tr.line-uncovered td.lineno { border-right-color: #dc3545; }
tr.line-uncovered td.code { background: #f8d7da; }
tr.line-ignored td.code { color: #adb5bd; }
</style>
</head>
<body>
"#);

    out.push_str(&format!(
        r#"<div class="header">
  <h1>Rust Compiler Coverage Report</h1>
  <div class="overall">{overall_pct:.1}% ({covered_lines_total}/{tracked_lines_total} lines)</div>
  <div class="overall-sub">{total} functions — {fully_count} fully covered, {partial_count} partial, {uncovered_count} uncovered</div>
</div>
<div class="toolbar">
  <span>Filter:</span>
  <button class="btn-all active" onclick="show('all',this)">All</button>
  <button class="btn-fully" onclick="show('fully',this)">Fully covered</button>
  <button class="btn-partial" onclick="show('partial',this)">Partial</button>
  <button class="btn-uncovered" onclick="show('uncovered',this)">Uncovered</button>
  <input type="text" placeholder="Search functions or paths..." oninput="onSearch(this.value)" />
  <span class="search-count" id="search-count"></span>
</div>
<script>
var currentCat = 'all';
var currentSearch = '';
var fnIdx = 0;

function applyFilters() {{
  var query = currentSearch.toLowerCase();
  var visible = 0;
  document.querySelectorAll('.fn-row').forEach(function(el) {{
    var catMatch = currentCat === 'all' || el.dataset.cat === currentCat;
    var searchMatch = query === '' || el.dataset.name.includes(query) || el.dataset.file.includes(query);
    var hide = !(catMatch && searchMatch);
    el.classList.toggle('hidden', hide);
    var src = el.nextElementSibling;
    if (src && src.classList.contains('source-view')) {{
      if (hide) src.classList.remove('open');
    }}
    if (!hide) visible++;
  }});
  var countEl = document.getElementById('search-count');
  if (countEl) countEl.textContent = query || currentCat !== 'all' ? visible + ' shown' : '';
}}

function show(cat, btn) {{
  document.querySelectorAll('.toolbar button').forEach(function(b) {{ b.classList.remove('active'); }});
  btn.classList.add('active');
  currentCat = cat;
  applyFilters();
}}

function onSearch(val) {{
  currentSearch = val.toLowerCase();
  applyFilters();
}}

function toggleFn(id) {{
  var src = document.getElementById('src-' + id);
  if (src) src.classList.toggle('open');
}}

function toggleCrate(id) {{
  var el = document.getElementById('crate-fns-' + id);
  if (el) el.classList.toggle('collapsed');
}}

function toggleModule(id) {{
  var el = document.getElementById('mod-fns-' + id);
  if (el) el.classList.toggle('collapsed');
}}
</script>
"#,
        overall_pct = overall_pct,
        covered_lines_total = covered_lines_total,
        tracked_lines_total = tracked_lines_total,
        total = total,
        fully_count = fully_count,
        partial_count = partial_count,
        uncovered_count = uncovered_count,
    ));

    // group by crate then module
    // collect: crate -> module -> [(report, cat)]
    let mut groups: std::collections::BTreeMap<String, std::collections::BTreeMap<String, Vec<(&FunctionReport, FunctionCategory)>>> = std::collections::BTreeMap::new();
    for (report, cat) in categorised {
        let (krate, module) = crate_and_module(&report.filename);
        groups.entry(krate).or_default().entry(module).or_default().push((report, *cat));
    }

    let mut crate_id = 0usize;
    let mut fn_id = 0usize;

    for (krate, modules) in &groups {
        let crate_total = modules.values().map(|v| v.len()).sum::<usize>();
        let crate_covered = modules.values().flat_map(|v| v.iter()).filter(|(_, c)| *c == FunctionCategory::FullyCovered).count();
        let crate_pct = pct(crate_covered, crate_total);

        out.push_str(&format!(
            r#"<div class="crate-block"><div class="crate-header" onclick="toggleCrate({cid})">▾ {krate} <span style="font-weight:normal;color:#6c757d;font-size:0.85em">({crate_covered}/{crate_total} fully covered, {crate_pct:.0}%)</span></div><div class="crate-fns" id="crate-fns-{cid}">"#,
            cid = crate_id,
            krate = escape(krate),
            crate_covered = crate_covered,
            crate_total = crate_total,
            crate_pct = crate_pct,
        ));

        let mut mod_id = crate_id * 10000;
        for (module, fns) in modules {
            let mod_total = fns.len();
            let mod_covered = fns.iter().filter(|(_, c)| *c == FunctionCategory::FullyCovered).count();
            let mod_pct = pct(mod_covered, mod_total);

            out.push_str(&format!(
                r#"<div class="module-header" onclick="toggleModule({mid})">▾ {module} <span style="font-weight:normal;color:#adb5bd">({mod_covered}/{mod_total}, {mod_pct:.0}%)</span></div><div class="module-fns" id="mod-fns-{mid}">"#,
                mid = mod_id,
                module = escape(module),
                mod_covered = mod_covered,
                mod_total = mod_total,
                mod_pct = mod_pct,
            ));

            for (report, cat) in fns {
                let cat_str = match cat {
                    FunctionCategory::FullyCovered => "fully",
                    FunctionCategory::PartiallyCovered => "partial",
                    FunctionCategory::FullyUncovered => "uncovered",
                };
                let dot_class = match cat {
                    FunctionCategory::FullyCovered => "dot-fully",
                    FunctionCategory::PartiallyCovered => "dot-partial",
                    FunctionCategory::FullyUncovered => "dot-uncovered",
                };
                let pct_class = match cat {
                    FunctionCategory::FullyCovered => "pct-fully",
                    FunctionCategory::PartiallyCovered => "pct-partial",
                    FunctionCategory::FullyUncovered => "pct-uncovered",
                };
                let covered_lines = report.line_counts.iter().filter(|c| c.map_or(false, |n| n > 0)).count();
                let total_tracked = report.line_counts.iter().filter(|c| c.is_some()).count();
                let fn_pct = if total_tracked > 0 { pct(covered_lines, total_tracked) } else { 100.0 };
                let pct_str = match cat {
                    FunctionCategory::FullyCovered => "100%".to_string(),
                    FunctionCategory::FullyUncovered => "0%".to_string(),
                    FunctionCategory::PartiallyCovered => format!("{fn_pct:.0}%"),
                };
                let short_file = if let Some(idx) = report.filename.find("/compiler/") {
                    &report.filename[idx + 1..]
                } else {
                    &report.filename
                };

                out.push_str(&format!(
                    r#"<div class="fn-row cat-{cat_str}" data-cat="{cat_str}" data-name="{name_lower}" data-file="{file_lower}" onclick="toggleFn({fid})"><span class="fn-dot {dot_class}"></span><span class="fn-name" title="{fn_name_full}">{fn_name}</span><span class="fn-pct {pct_class}">{pct_str}</span></div>"#,
                    cat_str = cat_str,
                    name_lower = escape(&report.demangled.to_lowercase()),
                    file_lower = escape(&short_file.to_lowercase()),
                    fid = fn_id,
                    dot_class = dot_class,
                    fn_name_full = escape(&report.demangled),
                    fn_name = escape(&report.demangled),
                    pct_class = pct_class,
                    pct_str = pct_str,
                ));

                // source view (hidden by default, toggled on click)
                out.push_str(&format!(r#"<div class="source-view" id="src-{fid}"><table class="src">"#, fid = fn_id));

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
                out.push_str("</table></div>\n");
                fn_id += 1;
            }

            out.push_str("</div>\n");
            mod_id += 1;
        }

        out.push_str("</div></div>\n");
        crate_id += 1;
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
