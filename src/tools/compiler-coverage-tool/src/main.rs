use std::path::PathBuf;
use std::process::Command;
use anyhow::{bail, Context, Result};
use rustc_demangle::demangle;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: {} <llvm-profdata> <combined.profdata> <output.html>", args[0]);
        std::process::exit(1);
    }

    let llvm_profdata = PathBuf::from(&args[1]);
    let profdata_path = PathBuf::from(&args[2]);
    let output_path = PathBuf::from(&args[3]);

    let output = Command::new(&llvm_profdata)
        .args(["show", "--all-functions", "--text"])
        .arg(&profdata_path)
        .output()
        .with_context(|| format!("failed to run {}", llvm_profdata.display()))?;

    if !output.status.success() {
        bail!(
            "llvm-profdata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let records = parse_text_profile(&text);

    let mut fully_covered: Vec<String> = vec![];
    let mut partially_covered: Vec<(String, usize, usize)> = vec![];
    let mut uncovered: Vec<String> = vec![];

    for (name, counts) in records {
        let demangled = format!("{:#}", demangle(&name));

        // only include rustc* crates
        if !demangled.starts_with("rustc") {
            continue;
        }

        let total = counts.len();
        let hit = counts.iter().filter(|&&c| c > 0).count();

        if hit == 0 {
            uncovered.push(demangled);
        } else if hit == total {
            fully_covered.push(demangled);
        } else {
            partially_covered.push((demangled, hit, total));
        }
    }

    fully_covered.sort();
    partially_covered.sort_by(|a, b| a.0.cmp(&b.0));
    uncovered.sort();

    let total = fully_covered.len() + partially_covered.len() + uncovered.len();
    let html = render_html(&fully_covered, &partially_covered, &uncovered, total);

    std::fs::write(&output_path, html)
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    println!(
        "written to {}\n  fully covered:  {}\n  partially:      {}\n  uncovered:      {}\n  total:          {}",
        output_path.display(),
        fully_covered.len(),
        partially_covered.len(),
        uncovered.len(),
        total,
    );

    Ok(())
}

fn parse_text_profile(text: &str) -> Vec<(String, Vec<u64>)> {
    let mut records = vec![];
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // function name line
        let name = line.to_string();

        // skip "# Func Hash:" and value
        skip_comment_and_value(&mut lines);
        // skip "# Num Counters:" and value
        let num_counters: usize = skip_comment_and_value(&mut lines)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // skip "# Counter Values:" header
        if let Some(l) = lines.next() {
            let _ = l;
        }
        // read counter values
        let mut counts = vec![];
        for _ in 0..num_counters {
            if let Some(l) = lines.peek() {
                let l = l.trim();
                if l.starts_with('#') || l.is_empty() {
                    break;
                }
                if let Ok(v) = l.parse::<u64>() {
                    counts.push(v);
                    lines.next();
                } else {
                    break;
                }
            }
        }
        if !counts.is_empty() {
            records.push((name, counts));
        }
    }

    records
}

fn skip_comment_and_value<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) -> Option<String> {
    // skip the "# Label:" line
    lines.next();
    // read the value line
    lines.next().map(|l| l.trim().to_string())
}

fn render_html(
    fully: &[String],
    partial: &[(String, usize, usize)],
    uncovered: &[String],
    total: usize,
) -> String {
    let fully_pct = pct(fully.len(), total);
    let partial_pct = pct(partial.len(), total);
    let uncovered_pct = pct(uncovered.len(), total);

    let fully_rows = fully
        .iter()
        .map(|n| format!("<tr><td class='fn'>{}</td></tr>", escape(n)))
        .collect::<Vec<_>>()
        .join("\n");

    let partial_rows = partial
        .iter()
        .map(|(n, hit, tot)| {
            format!(
                "<tr><td class='fn'>{}</td><td class='counts'>{}/{}</td></tr>",
                escape(n),
                hit,
                tot
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let uncovered_rows = uncovered
        .iter()
        .map(|n| format!("<tr><td class='fn'>{}</td></tr>", escape(n)))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Compiler Coverage Report</title>
<style>
  body {{ font-family: monospace; margin: 2em; background: #1e1e1e; color: #d4d4d4; }}
  h1 {{ color: #9cdcfe; }}
  .summary {{ display: flex; gap: 2em; margin-bottom: 2em; }}
  .box {{ padding: 1em 2em; border-radius: 4px; text-align: center; }}
  .green {{ background: #1e3a1e; border: 1px solid #4caf50; }}
  .yellow {{ background: #3a3a1e; border: 1px solid #ffeb3b; }}
  .red {{ background: #3a1e1e; border: 1px solid #f44336; }}
  .box .num {{ font-size: 2em; font-weight: bold; }}
  .box .label {{ font-size: 0.85em; color: #888; }}
  details {{ margin-bottom: 1em; }}
  summary {{ cursor: pointer; padding: 0.5em; border-radius: 3px; font-size: 1.1em; }}
  .green-hdr {{ background: #1e3a1e; }}
  .yellow-hdr {{ background: #3a3a1e; }}
  .red-hdr {{ background: #3a1e1e; }}
  table {{ width: 100%; border-collapse: collapse; }}
  tr:nth-child(even) {{ background: #2a2a2a; }}
  td {{ padding: 0.3em 0.5em; font-size: 0.85em; word-break: break-all; }}
  .counts {{ width: 6em; text-align: right; color: #888; }}
</style>
</head>
<body>
<h1>Compiler Coverage Report</h1>
<div class="summary">
  <div class="box green">
    <div class="num">{fully_count}</div>
    <div class="label">Fully Covered ({fully_pct:.1}%)</div>
  </div>
  <div class="box yellow">
    <div class="num">{partial_count}</div>
    <div class="label">Partially Covered ({partial_pct:.1}%)</div>
  </div>
  <div class="box red">
    <div class="num">{uncovered_count}</div>
    <div class="label">Uncovered ({uncovered_pct:.1}%)</div>
  </div>
</div>

<details open>
  <summary class="yellow-hdr">Partially Covered ({partial_count})</summary>
  <table>
    <tr><th>Function</th><th>Hit/Total</th></tr>
    {partial_rows}
  </table>
</details>

<details>
  <summary class="red-hdr">Uncovered ({uncovered_count})</summary>
  <table>
    {uncovered_rows}
  </table>
</details>

<details>
  <summary class="green-hdr">Fully Covered ({fully_count})</summary>
  <table>
    {fully_rows}
  </table>
</details>

</body>
</html>"#,
        fully_count = fully.len(),
        partial_count = partial.len(),
        uncovered_count = uncovered.len(),
        fully_pct = fully_pct,
        partial_pct = partial_pct,
        uncovered_pct = uncovered_pct,
        fully_rows = fully_rows,
        partial_rows = partial_rows,
        uncovered_rows = uncovered_rows,
    )
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
