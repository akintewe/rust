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

pub struct FunctionReport {
    pub demangled: String,
    pub filename: String,
    pub line_start: usize,
    pub _line_end: usize,
    // per-line hit counts for lines line_start..=line_end
    // None = LLVM doesn't track this line (closing braces etc)
    // Some(n) = total hits across all merged monomorphizations
    pub line_counts: Vec<Option<u64>>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum FunctionCategory {
    FullyCovered,
    PartiallyCovered,
    FullyUncovered,
}

/// Parses the llvm-cov export JSON, filters to compiler functions, merges
/// monomorphizations and closures into their parents, and returns the final
/// reports plus the source file cache used to build them (the cache is
/// reused later for the html source view, no need to re-read files).
pub fn process(json_text: &str, src_root: &Path) -> Result<(Vec<FunctionReport>, HashMap<String, Vec<String>>)> {
    let export: Export = serde_json::from_str(json_text)
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
            let resolved = resolve_source_path(&filename, src_root);
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

    // group by (filename, line_start) and sum hit counts across monomorphizations
    let reports = merge_monomorphizations(reports);
    eprintln!("{} functions after merging monomorphizations", reports.len());

    let reports = merge_closures(reports);
    eprintln!("{} functions after merging closures into parents", reports.len());

    Ok((reports, source_cache))
}

pub fn resolve_source_path(filename: &str, src_root: &Path) -> Option<PathBuf> {
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

fn closure_parent(name: &str) -> Option<&str> {
    // strip the last "::{closure#N}" (or "::{closure_env#N}") suffix
    // e.g. "foo::{closure#0}" -> "foo"
    //      "foo::{closure#0}::{closure#1}" -> "foo::{closure#0}"
    let idx = name.rfind("::{closure")?;
    Some(&name[..idx])
}

fn merge_closures(reports: Vec<FunctionReport>) -> Vec<FunctionReport> {
    // group by (filename, parent_name) — closures fold into their parent
    // key: (filename, canonical_name) where canonical_name strips closure suffixes
    let mut groups: std::collections::BTreeMap<(String, String), FunctionReport> =
        std::collections::BTreeMap::new();

    for report in reports {
        // walk up the closure chain to find the root parent name
        let mut root = report.demangled.as_str();
        while let Some(parent) = closure_parent(root) {
            root = parent;
        }
        let key = (report.filename.clone(), root.to_string());

        let root_owned = root.to_string();
        match groups.get_mut(&key) {
            None => {
                let mut r = report;
                r.demangled = root_owned;
                groups.insert(key, r);
            }
            Some(existing) => {
                // `line_counts[i]` means line `line_start + i` for EACH report's own
                // span -- a closure's span rarely matches its parent's, so merging by
                // vec index silently summed/appended unrelated lines. Re-key both
                // sides by actual line number into the union of both spans instead.
                let new_line_start = existing.line_start.min(report.line_start);
                let existing_line_end = existing.line_start + existing.line_counts.len();
                let report_line_end = report.line_start + report.line_counts.len();
                let new_line_end = existing_line_end.max(report_line_end);

                let mut merged: Vec<Option<u64>> =
                    vec![None; new_line_end.saturating_sub(new_line_start)];

                let place = |line_start: usize, counts: &[Option<u64>], merged: &mut Vec<Option<u64>>| {
                    for (i, count) in counts.iter().enumerate() {
                        let lineno = line_start + i;
                        let idx = lineno - new_line_start;
                        merged[idx] = match (merged[idx], *count) {
                            (Some(a), Some(b)) => Some(a.saturating_add(b)),
                            (Some(a), None) => Some(a),
                            (None, Some(b)) => Some(b),
                            (None, None) => None,
                        };
                    }
                };
                place(existing.line_start, &existing.line_counts, &mut merged);
                place(report.line_start, &report.line_counts, &mut merged);

                existing.line_start = new_line_start;
                existing.line_counts = merged;
            }
        }
    }

    groups.into_values().collect()
}
