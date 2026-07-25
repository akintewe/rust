use std::collections::HashMap;
use std::path::Path;
use anyhow::{Context, Result};
use serde::Serialize;

use crate::processing::{FunctionCategory, FunctionReport};

#[derive(Serialize)]
struct SourceLine<'a> {
    #[serde(rename = "n")]
    lineno: usize,
    #[serde(rename = "c")]
    class: &'static str,
    #[serde(rename = "t")]
    text: &'a str,
}

const SHARD_COUNT: usize = 16;

/// Writes each function's source lines to one of `SHARD_COUNT` json files,
/// keyed by function id. The html loads a shard on demand the first time a
/// function's <details> is opened, instead of embedding all source inline.
pub fn write_source_shards(
    categorised: &[(usize, &FunctionReport, FunctionCategory)],
    source_cache: &HashMap<String, Vec<String>>,
    shard_dir: &Path,
) -> Result<()> {
    std::fs::create_dir_all(shard_dir)
        .with_context(|| format!("failed to create {}", shard_dir.display()))?;

    let mut shards: Vec<HashMap<usize, Vec<SourceLine<'_>>>> =
        (0..SHARD_COUNT).map(|_| HashMap::new()).collect();

    for (fn_id, report, _) in categorised {
        let source_lines = source_cache.get(&report.filename);
        let lines: Vec<SourceLine<'_>> = report.line_counts.iter().enumerate().map(|(i, count)| {
            let lineno = report.line_start + i;
            let class = match count {
                Some(n) if *n > 0 => "c",
                Some(_) => "u",
                None => "i",
            };
            let text = source_lines
                .and_then(|ls| ls.get(lineno.saturating_sub(1)))
                .map(|s| s.as_str())
                .unwrap_or("");
            SourceLine { lineno, class, text }
        }).collect();
        shards[fn_id % SHARD_COUNT].insert(*fn_id, lines);
    }

    for (i, shard) in shards.iter().enumerate() {
        let shard_path = shard_dir.join(format!("shard-{i}.json"));
        let json = serde_json::to_string(shard)
            .with_context(|| format!("failed to serialize shard {i}"))?;
        std::fs::write(&shard_path, json)
            .with_context(|| format!("failed to write {}", shard_path.display()))?;
    }

    Ok(())
}

// builds a github.com/<owner>/<repo>/blob/<sha> url from the origin remote
// and current commit, or None if src_root isn't a github checkout
pub fn github_base_url(src_root: &Path) -> Option<String> {
    let remote_out = std::process::Command::new("git")
        .args(["-C", &src_root.to_string_lossy(), "remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !remote_out.status.success() {
        return None;
    }
    let remote_url = String::from_utf8_lossy(&remote_out.stdout).trim().to_string();

    // handle both "https://github.com/owner/repo.git" and "git@github.com:owner/repo.git"
    let owner_repo = if let Some(rest) = remote_url.strip_prefix("https://github.com/") {
        rest.trim_end_matches(".git").to_string()
    } else if let Some(rest) = remote_url.strip_prefix("git@github.com:") {
        rest.trim_end_matches(".git").to_string()
    } else {
        return None;
    };

    let sha_out = std::process::Command::new("git")
        .args(["-C", &src_root.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !sha_out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();

    Some(format!("https://github.com/{owner_repo}/blob/{sha}"))
}

pub fn render_html(
    categorised: &[(usize, &FunctionReport, FunctionCategory)],
    fully_count: usize,
    partial_count: usize,
    uncovered_count: usize,
    total: usize,
    shard_dir_name: &str,
    github_base: &Option<String>,
) -> String {
    let covered_lines_total: usize = categorised.iter().map(|(_, r, _)| {
        r.line_counts.iter().filter(|c| c.map_or(false, |n| n > 0)).count()
    }).sum();
    let tracked_lines_total: usize = categorised.iter().map(|(_, r, _)| {
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
.crate-group { margin: 0; }
.crate-group > summary { display: flex; align-items: center; gap: 0.4em; list-style: none; cursor: pointer; padding: 0.4em 2em; background: #f1f3f5; border-bottom: 1px solid #dee2e6; font-weight: 600; font-size: 0.88em; user-select: none; }
.crate-group > summary:hover { background: #e9ecef; }
.crate-group > summary::-webkit-details-marker { display: none; }
.crate-group > summary::before { content: "▶"; font-size: 0.7em; color: #6c757d; transition: transform 0.12s; }
.crate-group[open] > summary::before { transform: rotate(90deg); }
.crate-count { font-weight: normal; color: #6c757d; font-size: 0.9em; }
.file-group { margin: 0; }
.file-group > summary { display: flex; align-items: center; gap: 0.4em; list-style: none; cursor: pointer; padding: 0.3em 2em 0.3em 3.5em; background: #f8f9fa; border-bottom: 1px solid #eee; font-size: 0.83em; color: #495057; font-family: "SFMono-Regular", Consolas, monospace; user-select: none; }
.file-group > summary:hover { background: #f0f0f0; }
.file-group > summary::-webkit-details-marker { display: none; }
.file-group > summary::before { content: "▶"; font-size: 0.65em; color: #adb5bd; transition: transform 0.12s; }
.file-group[open] > summary::before { transform: rotate(90deg); }
.fn-list { padding: 0 2em; }
.fn-block { margin: 0; }
.fn-block.hidden { display: none; }
details > summary {
  padding: 0.3em 0; cursor: pointer; list-style: none;
  display: flex; align-items: baseline; gap: 0.5em; user-select: none;
}
details > summary:hover .fn-name { text-decoration: underline; }
details > summary::-webkit-details-marker { display: none; }
details > summary::before { content: "▶"; font-size: 0.7em; color: #adb5bd; flex-shrink: 0; transition: transform 0.12s; }
details[open] > summary::before { transform: rotate(90deg); }
.fn-name { font-size: 0.9em; font-weight: 600; color: #e36d00; word-break: break-all; font-family: "SFMono-Regular", Consolas, monospace; flex: 1; }
.fn-badge {
  font-size: 0.8em; font-weight: 600; padding: 0.15em 0.5em;
  border-radius: 3px; white-space: nowrap; font-family: inherit; flex-shrink: 0;
}
.badge-fully { background: #d1e7dd; color: #0a3622; }
.badge-partial { background: #ffe5d0; color: #6c2a00; }
.badge-uncovered { background: #f8d7da; color: #58151c; }
.fn-file { font-size: 0.8em; color: #6c757d; padding: 0.2em 0 0.4em 1.2em; font-family: "SFMono-Regular", Consolas, monospace; }
.source-view { overflow-x: auto; background: #fdfdfd; border-left: 2px solid #dee2e6; margin: 0 0 0.5em 0.6em; }
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
  document.querySelectorAll('.file-group').forEach(el => {{
    el.style.display = el.querySelector('.fn-block:not(.hidden)') ? '' : 'none';
  }});
  document.querySelectorAll('.crate-group').forEach(el => {{
    el.style.display = el.querySelector('.fn-block:not(.hidden)') ? '' : 'none';
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

// Source lines are split across shard-N.json files (see write_source_shards
// in render.rs) instead of embedded in the html -- fetched lazily the first
// time a function's <details> is opened, cached in memory after that.
var SHARD_COUNT = {shard_count};
var SHARD_DIR = '{shard_dir_name}';
var shardCache = {{}};
var shardPromises = {{}};

function loadSource(details) {{
  if (!details.open) return;
  if (details.getAttribute('data-loaded') === '1') return;
  var fnId = details.getAttribute('data-fn-id');
  var body = details.querySelector('.src-body');
  var shardKey = Number(fnId) % SHARD_COUNT;

  var fetchPromise = shardPromises[shardKey];
  if (!fetchPromise) {{
    fetchPromise = fetch(SHARD_DIR + '/shard-' + shardKey + '.json')
      .then(r => r.json())
      .then(data => {{ shardCache[shardKey] = data; return data; }});
    shardPromises[shardKey] = fetchPromise;
  }}

  fetchPromise.then(data => {{
    var lines = data[fnId];
    if (!lines) {{
      body.innerHTML = '<tr><td class="code">(source unavailable)</td></tr>';
      return;
    }}
    var classMap = {{ c: 'line-covered', u: 'line-uncovered', i: 'line-ignored' }};
    var html = '';
    for (var i = 0; i < lines.length; i++) {{
      var ln = lines[i];
      var cls = classMap[ln.c] || 'line-ignored';
      html += '<tr class="' + cls + '"><td class="lineno">' + ln.n + '</td><td class="code">' + escapeHtml(ln.t) + '</td></tr>';
    }}
    body.innerHTML = html;
    details.setAttribute('data-loaded', '1');
  }}).catch(err => {{
    body.innerHTML = '<tr><td class="code">(failed to load source: ' + escapeHtml(String(err)) + ')</td></tr>';
  }});
}}

function escapeHtml(s) {{
  var div = document.createElement('div');
  div.textContent = s;
  return div.innerHTML;
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
        shard_count = SHARD_COUNT,
        shard_dir_name = shard_dir_name,
    ));

    // render in order: uncovered first (most interesting), then partial, then fully
    let order = [
        (FunctionCategory::FullyUncovered, "uncovered", "Fully Uncovered", uncovered_count),
        (FunctionCategory::PartiallyCovered, "partial", "Partially Covered", partial_count),
        (FunctionCategory::FullyCovered, "fully", "Fully Covered", fully_count),
    ];

    for (cat_variant, cat_class, label, count) in &order {
        out.push_str(&format!(
            "<div class=\"section-header {cat_class}\">{label} ({count})</div>\n",
            cat_class = cat_class,
            label = label,
            count = count,
        ));

        let section_fns: Vec<&(usize, &FunctionReport, FunctionCategory)> = categorised
            .iter()
            .filter(|(_, _, cat)| cat == cat_variant)
            .collect();

        let mut seen_crates: Vec<String> = vec![];
        for (_, report, _) in &section_fns {
            let krate = crate_name(&report.filename);
            if !seen_crates.contains(&krate) {
                seen_crates.push(krate);
            }
        }

        for krate in &seen_crates {
            let krate_fns: Vec<&&(usize, &FunctionReport, FunctionCategory)> = section_fns
                .iter()
                .filter(|(_, r, _)| &crate_name(&r.filename) == krate)
                .collect();
            let krate_count = krate_fns.len();

            out.push_str(&format!(
                "<details class=\"crate-group\"><summary class=\"crate-header\">{krate} <span class=\"crate-count\">({krate_count})</span></summary>\n",
                krate = escape(krate),
                krate_count = krate_count,
            ));

            // nest by file path within the crate
            let mut seen_files: Vec<String> = vec![];
            for (_, report, _) in &krate_fns {
                let f = file_path_in_crate(&report.filename);
                if !seen_files.contains(&f) { seen_files.push(f); }
            }

            for file in &seen_files {
                let file_fns: Vec<&&&(usize, &FunctionReport, FunctionCategory)> = krate_fns
                    .iter()
                    .filter(|(_, r, _)| &file_path_in_crate(&r.filename) == file)
                    .collect();
                let file_count = file_fns.len();

                out.push_str(&format!(
                    "<details class=\"file-group\"><summary class=\"file-header\">{file} <span class=\"crate-count\">({file_count})</span></summary>\n<div class=\"fn-list\">\n",
                    file = escape(file),
                    file_count = file_count,
                ));

        for (fn_id, report, _) in &file_fns {
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

            // Source lines load lazily from a JSON shard when this <details>
            // is first opened -- see loadSource() in the script above. Embedding
            // every function's source inline made reports ~77MB; this keeps the
            // initial HTML to just the collapsed summary rows.
            //
            // The file:line label links out to GitHub (using the origin remote
            // and commit the report was built from) when one was found, so a
            // maintainer can jump straight to real context around the function
            // instead of the isolated lines this report shows.
            let file_line_html = match github_base {
                Some(base) => format!(
                    r#"<a href="{base}/{path}#L{line}" target="_blank" rel="noopener">{short_file}:{line_start}</a>"#,
                    base = base,
                    path = escape(short_filename),
                    line = report.line_start,
                    short_file = escape(short_filename),
                    line_start = report.line_start,
                ),
                None => format!("{}:{}", escape(short_filename), report.line_start),
            };

            out.push_str(&format!(
                r#"<div class="fn-block cat-{cat_class}">
<details data-fn-id="{fn_id}" ontoggle="loadSource(this)">
<summary><span class="fn-name">{fn_name}</span><span class="fn-badge {badge_class}">{badge_text}</span></summary>
<div class="fn-file">{file_line_html}</div>
<div class="source-view"><table class="src"><tbody class="src-body"><tr><td class="code src-loading">loading...</td></tr></tbody></table></div>
</details></div>
"#,
                cat_class = cat_class,
                fn_id = fn_id,
                fn_name = escape(&report.demangled),
                file_line_html = file_line_html,
                badge_class = badge_class,
                badge_text = badge_text,
            ));
        }

            out.push_str("</div></details>\n"); // close file-group
            }

        out.push_str("</details>\n"); // close crate-group
        }
    }

    out.push_str("</body></html>\n");
    out
}

fn crate_name(filename: &str) -> String {
    if let Some(idx) = filename.find("/compiler/") {
        let after = &filename[idx + "/compiler/".len()..];
        return after.split('/').next().unwrap_or("unknown").to_string();
    }
    "unknown".to_string()
}

fn file_path_in_crate(filename: &str) -> String {
    // "/.../compiler/rustc_abi/src/lib.rs" -> "src/lib.rs"
    if let Some(idx) = filename.find("/compiler/") {
        let after = &filename[idx + "/compiler/".len()..];
        if let Some(slash) = after.find('/') {
            return after[slash + 1..].to_string();
        }
    }
    filename.to_string()
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
