use std::collections::HashMap;
use std::path::Path;
use anyhow::{Context, Result};
use askama::Template;
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

const STYLE: &str = r#"
* { box-sizing: border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 0; background: #f8f9fa; color: #212529; }
.header { background: #fff; border-bottom: 1px solid #dee2e6; padding: 1.5em 2em; }
.header h1 { margin: 0 0 0.2em; font-size: 1.4em; color: #343a40; }
.header a { color: #343a40; text-decoration: none; }
.overall { font-size: 2.2em; font-weight: bold; color: #212529; margin: 0.2em 0; }
.overall-sub { color: #6c757d; font-size: 0.95em; }
.category-list { list-style: none; padding: 0 2em 1.5em; margin: 0; display: flex; flex-direction: column; gap: 0.6em; }
.category-list a {
  display: flex; justify-content: space-between; align-items: center;
  padding: 0.8em 1.2em; border-radius: 6px; text-decoration: none;
  font-size: 1.05em; font-weight: 600; color: #fff;
}
.category-list a.uncovered { background: #dc3545; }
.category-list a.partial { background: #fd7e14; }
.category-list a.fully { background: #198754; }
.category-list a span.pct { font-weight: normal; opacity: 0.9; font-size: 0.9em; }
.filter-bar { display: flex; gap: 0.5em; padding: 1em 2em; background: #fff; border-bottom: 1px solid #dee2e6; flex-wrap: wrap; align-items: center; }
.filter-bar span { color: #6c757d; font-size: 0.9em; margin-right: 0.5em; }
.filter-bar a {
  padding: 0.35em 1em; border-radius: 20px; border: 1px solid; cursor: pointer;
  font-size: 0.85em; font-family: inherit; background: #fff; text-decoration: none;
}
.search-bar { padding: 0.6em 2em; background: #fff; border-bottom: 1px solid #dee2e6; display: flex; align-items: center; gap: 0.5em; }
.search-bar input {
  padding: 0.4em 0.8em; border: 1px solid #ced4da; border-radius: 4px;
  font-size: 0.9em; font-family: "SFMono-Regular", Consolas, monospace;
  width: 30em; outline: none;
}
.search-bar input:focus { border-color: #86b7fe; box-shadow: 0 0 0 2px rgba(13,110,253,0.15); }
.search-bar .search-count { font-size: 0.85em; color: #6c757d; }
.btn-uncovered { border-color: #dc3545; color: #dc3545; }
.btn-uncovered.active { background: #dc3545; color: #fff; }
.btn-partial { border-color: #fd7e14; color: #fd7e14; }
.btn-partial.active { background: #fd7e14; color: #fff; }
.btn-fully { border-color: #198754; color: #198754; }
.btn-fully.active { background: #198754; color: #fff; }
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
"#;

const SOURCE_LOADER_SCRIPT: &str = r#"
var shardCache = {};
var shardPromises = {};

function loadSource(details) {
  if (!details.open) return;
  if (details.getAttribute('data-loaded') === '1') return;
  var fnId = details.getAttribute('data-fn-id');
  var body = details.querySelector('.src-body');
  var shardKey = Number(fnId) % SHARD_COUNT;

  var fetchPromise = shardPromises[shardKey];
  if (!fetchPromise) {
    fetchPromise = fetch(SHARD_DIR + '/shard-' + shardKey + '.json')
      .then(r => r.json())
      .then(data => { shardCache[shardKey] = data; return data; });
    shardPromises[shardKey] = fetchPromise;
  }

  fetchPromise.then(data => {
    var lines = data[fnId];
    if (!lines) {
      body.innerHTML = '<tr><td class="code">(source unavailable)</td></tr>';
      return;
    }
    var classMap = { c: 'line-covered', u: 'line-uncovered', i: 'line-ignored' };
    var html = '';
    for (var i = 0; i < lines.length; i++) {
      var ln = lines[i];
      var cls = classMap[ln.c] || 'line-ignored';
      html += '<tr class="' + cls + '"><td class="lineno">' + ln.n + '</td><td class="code">' + escapeHtml(ln.t) + '</td></tr>';
    }
    body.innerHTML = html;
    details.setAttribute('data-loaded', '1');
  }).catch(err => {
    body.innerHTML = '<tr><td class="code">(failed to load source: ' + escapeHtml(String(err)) + ')</td></tr>';
  });
}

function escapeHtml(s) {
  var div = document.createElement('div');
  div.textContent = s;
  return div.innerHTML;
}
"#;

const SEARCH_SCRIPT: &str = r#"
var currentSearch = '';

function applyFilters() {
  var query = currentSearch.toLowerCase();
  var visible = 0;
  document.querySelectorAll('.fn-block').forEach(el => {
    var name = el.querySelector('.fn-name') ? el.querySelector('.fn-name').textContent.toLowerCase() : '';
    var file = el.querySelector('.fn-file') ? el.querySelector('.fn-file').textContent.toLowerCase() : '';
    var hide = query !== '' && !name.includes(query) && !file.includes(query);
    el.classList.toggle('hidden', hide);
    if (!hide) visible++;
  });
  document.querySelectorAll('.file-group').forEach(el => {
    el.style.display = el.querySelector('.fn-block:not(.hidden)') ? '' : 'none';
  });
  document.querySelectorAll('.crate-group').forEach(el => {
    el.style.display = el.querySelector('.fn-block:not(.hidden)') ? '' : 'none';
  });
  var countEl = document.getElementById('search-count');
  if (countEl) countEl.textContent = query ? visible + ' result' + (visible === 1 ? '' : 's') : '';
}

function onSearch(val) {
  currentSearch = val;
  applyFilters();
}
"#;

/// Filenames for the report: one index page plus one page per coverage
/// category, so opening the report doesn't load a single html file with
/// every function in the compiler in it. `base_name` is the output file's
/// stem (e.g. "report" -> report.html, report_uncovered.html, ...).
pub struct ReportPaths {
    pub index: String,
    pub fully_covered: String,
    pub partially_covered: String,
    pub uncovered: String,
}

pub fn report_paths(base_name: &str) -> ReportPaths {
    ReportPaths {
        index: format!("{base_name}.html"),
        fully_covered: format!("{base_name}_fully-covered.html"),
        partially_covered: format!("{base_name}_partially-covered.html"),
        uncovered: format!("{base_name}_uncovered.html"),
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate<'a> {
    style: &'static str,
    overall_pct: String,
    covered_lines_total: usize,
    tracked_lines_total: usize,
    uncovered_path: &'a str,
    partial_path: &'a str,
    fully_path: &'a str,
    uncovered_count: usize,
    partial_count: usize,
    fully_count: usize,
}

/// Small overview page: overall stats and links to the three category
/// pages. Kept separate from the category pages themselves so opening the
/// report doesn't pull in the full function tree just to see the summary.
pub fn render_index(
    fully_count: usize,
    partial_count: usize,
    uncovered_count: usize,
    total: usize,
    covered_lines_total: usize,
    tracked_lines_total: usize,
    paths: &ReportPaths,
) -> Result<String> {
    let _ = total;
    let template = IndexTemplate {
        style: STYLE,
        overall_pct: format!("{:.2}", pct(covered_lines_total, tracked_lines_total)),
        covered_lines_total,
        tracked_lines_total,
        uncovered_path: &paths.uncovered,
        partial_path: &paths.partially_covered,
        fully_path: &paths.fully_covered,
        uncovered_count,
        partial_count,
        fully_count,
    };
    Ok(template.render()?)
}

#[derive(Template)]
#[template(path = "category.html")]
struct CategoryTemplate<'a> {
    style: &'static str,
    search_script: &'static str,
    source_loader_script: &'static str,
    label: &'a str,
    this_count: usize,
    cat_class: &'a str,
    index_path: &'a str,
    uncovered_path: &'a str,
    partial_path: &'a str,
    fully_path: &'a str,
    uncovered_active: bool,
    partial_active: bool,
    fully_active: bool,
    shard_count: usize,
    shard_dir_name: &'a str,
    crates: Vec<CrateGroup>,
}

/// One category's crate/file/function tree (e.g. just the fully-uncovered
/// functions). Rendering categories as separate pages instead of one big
/// page with all three keeps each page's function count -- and so its
/// size -- down to roughly a third of the old single-page report.
pub fn render_category_page(
    categorised: &[(usize, &FunctionReport, FunctionCategory)],
    cat_variant: FunctionCategory,
    cat_class: &str,
    label: &str,
    shard_dir_name: &str,
    github_base: &Option<String>,
    paths: &ReportPaths,
) -> Result<String> {
    let this_count = categorised.iter().filter(|(_, _, c)| *c == cat_variant).count();

    let section_fns: Vec<&(usize, &FunctionReport, FunctionCategory)> = categorised
        .iter()
        .filter(|(_, _, cat)| *cat == cat_variant)
        .collect();

    let crates = group_by_crate_and_file(&section_fns, cat_variant, github_base);

    let template = CategoryTemplate {
        style: STYLE,
        search_script: SEARCH_SCRIPT,
        source_loader_script: SOURCE_LOADER_SCRIPT,
        label,
        this_count,
        cat_class,
        index_path: &paths.index,
        uncovered_path: &paths.uncovered,
        partial_path: &paths.partially_covered,
        fully_path: &paths.fully_covered,
        uncovered_active: cat_class == "uncovered",
        partial_active: cat_class == "partial",
        fully_active: cat_class == "fully",
        shard_count: SHARD_COUNT,
        shard_dir_name,
        crates,
    };
    Ok(template.render()?)
}

struct FnRow {
    fn_id: usize,
    fn_name: String,
    badge_class: &'static str,
    badge_text: String,
    // Pre-built <a> tag (or plain text) linking to the function's source --
    // built once here since it's conditional on github_base, then rendered
    // with `|safe` since its pieces are already escaped below.
    file_line_html: String,
}

struct FileGroup {
    file: String,
    count: usize,
    fns: Vec<FnRow>,
}

struct CrateGroup {
    krate: String,
    count: usize,
    files: Vec<FileGroup>,
}

/// Groups one category's functions into a crate -> file -> function tree
/// for the template to walk. Kept separate from rendering so the grouping
/// logic can be tested without going through askama.
fn group_by_crate_and_file(
    section_fns: &[&(usize, &FunctionReport, FunctionCategory)],
    cat_variant: FunctionCategory,
    github_base: &Option<String>,
) -> Vec<CrateGroup> {
    let mut seen_crates: Vec<String> = vec![];
    for (_, report, _) in section_fns {
        let krate = crate_name(&report.filename);
        if !seen_crates.contains(&krate) {
            seen_crates.push(krate);
        }
    }

    seen_crates
        .into_iter()
        .map(|krate| {
            let krate_fns: Vec<&&(usize, &FunctionReport, FunctionCategory)> = section_fns
                .iter()
                .filter(|(_, r, _)| crate_name(&r.filename) == krate)
                .collect();

            let mut seen_files: Vec<String> = vec![];
            for (_, report, _) in &krate_fns {
                let f = file_path_in_crate(&report.filename);
                if !seen_files.contains(&f) {
                    seen_files.push(f);
                }
            }

            let files = seen_files
                .into_iter()
                .map(|file| {
                    let file_fns: Vec<&&&(usize, &FunctionReport, FunctionCategory)> = krate_fns
                        .iter()
                        .filter(|(_, r, _)| file_path_in_crate(&r.filename) == file)
                        .collect();

                    let fns = file_fns
                        .iter()
                        .map(|(fn_id, report, _)| {
                            build_fn_row(*fn_id, report, cat_variant, github_base)
                        })
                        .collect::<Vec<_>>();

                    FileGroup { count: fns.len(), file, fns }
                })
                .collect::<Vec<_>>();

            CrateGroup { count: krate_fns.len(), krate, files }
        })
        .collect()
}

fn build_fn_row(
    fn_id: usize,
    report: &FunctionReport,
    cat_variant: FunctionCategory,
    github_base: &Option<String>,
) -> FnRow {
    let short_filename = if let Some(idx) = report.filename.find("/compiler/") {
        &report.filename[idx + 1..]
    } else {
        &report.filename
    };

    let covered_lines = report.line_counts.iter().filter(|c| c.map_or(false, |n| n > 0)).count();
    let total_tracked = report.line_counts.iter().filter(|c| c.is_some()).count();
    let fn_pct = if total_tracked > 0 { pct(covered_lines, total_tracked) } else { 100.0 };

    let (badge_class, badge_text) = match cat_variant {
        FunctionCategory::FullyCovered => ("badge-fully", "100% covered".to_string()),
        FunctionCategory::PartiallyCovered => ("badge-partial", format!("{fn_pct:.0}% covered")),
        FunctionCategory::FullyUncovered => ("badge-uncovered", "0% covered".to_string()),
    };

    // Source lines load lazily from a JSON shard when this <details> is
    // first opened -- see loadSource() in SOURCE_LOADER_SCRIPT. Embedding
    // every function's source inline made reports ~77MB; this keeps each
    // page to just the collapsed summary rows.
    //
    // The file:line label links out to GitHub (using the origin remote and
    // commit the report was built from) when one was found, so a maintainer
    // can jump straight to real context around the function instead of the
    // isolated lines shown here.
    let file_line_html = match github_base {
        Some(base) => format!(
            r#"<a href="{base}/{path}#L{line}" target="_blank" rel="noopener">{short_file}:{line_start}</a>"#,
            base = base,
            path = html_escape(short_filename),
            line = report.line_start,
            short_file = html_escape(short_filename),
            line_start = report.line_start,
        ),
        None => format!("{}:{}", html_escape(short_filename), report.line_start),
    };

    FnRow {
        fn_id,
        fn_name: report.demangled.clone(),
        badge_class,
        badge_text,
        file_line_html,
    }
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

// Manual escaping for the pre-built anchor fragment in build_fn_row --
// askama auto-escapes everything else, but that fragment is assembled as a
// raw string and rendered with `|safe` since it needs to stay a real <a> tag.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processing::FunctionReport;

    fn make_report(demangled: &str, filename: &str, line_start: usize, line_counts: Vec<Option<u64>>) -> FunctionReport {
        let line_end = line_start + line_counts.len().saturating_sub(1);
        FunctionReport { demangled: demangled.to_string(), filename: filename.to_string(), line_start, _line_end: line_end, line_counts }
    }

    #[test]
    fn html_escape_handles_all_four_special_characters() {
        assert_eq!(html_escape(r#"<a href="x">&y</a>"#), "&lt;a href=&quot;x&quot;&gt;&amp;y&lt;/a&gt;");
    }

    #[test]
    fn crate_name_extracts_the_crate_directory() {
        assert_eq!(crate_name("/home/user/rust/compiler/rustc_abi/src/lib.rs"), "rustc_abi");
    }

    #[test]
    fn crate_name_is_unknown_outside_the_compiler_tree() {
        assert_eq!(crate_name("/home/user/somewhere/else.rs"), "unknown");
    }

    #[test]
    fn file_path_in_crate_strips_the_crate_name_prefix() {
        assert_eq!(file_path_in_crate("/home/user/rust/compiler/rustc_abi/src/lib.rs"), "src/lib.rs");
    }

    #[test]
    fn report_paths_produces_four_distinct_filenames() {
        let paths = report_paths("report");
        assert_eq!(paths.index, "report.html");
        assert_eq!(paths.fully_covered, "report_fully-covered.html");
        assert_eq!(paths.partially_covered, "report_partially-covered.html");
        assert_eq!(paths.uncovered, "report_uncovered.html");
        // all four must be distinct, or two categories would overwrite each other
        let all = [&paths.index, &paths.fully_covered, &paths.partially_covered, &paths.uncovered];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn index_page_links_to_the_exact_filenames_report_paths_returns() {
        // the html generator and report_paths must stay in sync -- if render_index
        // hardcoded a filename instead of using ReportPaths, this would catch it
        let paths = report_paths("report");
        let html = render_index(1, 2, 3, 6, 100, 200, &paths).unwrap();
        assert!(html.contains(&paths.uncovered), "index must link to the uncovered page");
        assert!(html.contains(&paths.partially_covered), "index must link to the partial page");
        assert!(html.contains(&paths.fully_covered), "index must link to the fully-covered page");
    }

    #[test]
    fn category_page_contains_the_function_name_and_source_view_placeholder() {
        let report = make_report("rustc_abi::callconv::merge", "/rust/compiler/rustc_abi/src/callconv.rs", 39, vec![Some(0)]);
        let categorised = vec![(0usize, &report, FunctionCategory::FullyUncovered)];
        let paths = report_paths("report");

        let html = render_category_page(
            &categorised,
            FunctionCategory::FullyUncovered,
            "uncovered",
            "Fully Uncovered",
            "report_sources",
            &None,
            &paths,
        ).unwrap();

        assert!(html.contains("rustc_abi::callconv::merge"), "function name must appear in its own page");
        assert!(html.contains("data-fn-id=\"0\""), "function needs a stable id for the source-shard lookup");
        // source lines aren't embedded -- only a loading placeholder that
        // loadSource() replaces client-side, see write_source_shards
        assert!(!html.contains("fn merge(self"), "source text must not be inlined in the page");
    }

    #[test]
    fn category_page_only_shows_functions_in_that_category() {
        let uncovered = make_report("a", "/rust/compiler/c/src/x.rs", 1, vec![Some(0)]);
        let covered = make_report("b", "/rust/compiler/c/src/x.rs", 5, vec![Some(1)]);
        let categorised = vec![
            (0usize, &uncovered, FunctionCategory::FullyUncovered),
            (1usize, &covered, FunctionCategory::FullyCovered),
        ];
        let paths = report_paths("report");

        let html = render_category_page(
            &categorised,
            FunctionCategory::FullyUncovered,
            "uncovered",
            "Fully Uncovered",
            "report_sources",
            &None,
            &paths,
        ).unwrap();

        assert!(html.contains('a'), "sanity: page was generated");
        assert!(!html.contains("data-fn-id=\"1\""), "the fully-covered function must not appear on the uncovered page");
    }
}
