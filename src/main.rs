//! The `jsonl-peek` CLI: a thin argument parser wired to the library.
//!
//! Argument parsing is hand rolled rather than pulled in from a crate: the
//! surface here is four subcommands and a dozen flags, well within what a
//! plain loop over `env::args` can handle cleanly, and it keeps the
//! dependency tree at zero per the crate's whole reason to exist.

use std::fs;
use std::io::{self, BufRead, Write};

use jsonl_peek::hist::Histogram;
use jsonl_peek::json;
use jsonl_peek::lines::LineReader;
use jsonl_peek::path::FieldPath;
use jsonl_peek::rng::{Reservoir, SplitMix64};
use jsonl_peek::schema::{Schema, SchemaOptions, MAX_PATHS};
use jsonl_peek::stats::{Stats, StatsOptions, TypeCounts};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&args));
}

fn run(args: &[String]) -> i32 {
    let Some((command, rest)) = args.split_first() else {
        print_usage();
        return 2;
    };
    match command.as_str() {
        "head" => run_head(rest),
        "sample" => run_sample(rest),
        "stats" => run_stats(rest),
        "schema" => run_schema(rest),
        "-h" | "--help" => {
            print_usage();
            0
        }
        other => {
            eprintln!("jsonl-peek: unknown command '{other}'");
            print_usage();
            2
        }
    }
}

fn print_usage() {
    eprintln!("usage: jsonl-peek <head|sample|stats|schema> [options] [FILE]");
    eprintln!();
    eprintln!("  jsonl-peek head   [-n N] [FILE]");
    eprintln!("  jsonl-peek sample [-n N] [--seed S] [FILE]");
    eprintln!("  jsonl-peek stats  [--field PATH]... [--top N] [--max-errors N] [--json] [FILE]");
    eprintln!("  jsonl-peek schema [--depth N] [--min-rate R] [--json] [FILE]");
}

fn run_head(args: &[String]) -> i32 {
    let mut n: usize = 10;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                let v = match arg_value(args, i, "head", "-n") {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                n = match parse_arg("head", "-n", v) {
                    Ok(n) => n,
                    Err(code) => return code,
                };
                i += 2;
            }
            "-h" | "--help" => {
                println!("usage: jsonl-peek head [-n N] [FILE]");
                return 0;
            }
            other if file.is_none() && !other.starts_with('-') => {
                file = Some(other.to_string());
                i += 1;
            }
            other => return usage_error("head", &format!("unexpected argument '{other}'")),
        }
    }

    let reader = match open_input(file.as_deref()) {
        Ok(r) => r,
        Err(err) => return runtime_error(&format!("{}: {err}", file.as_deref().unwrap_or("-"))),
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut lines = LineReader::new(reader);
    for _ in 0..n {
        match lines.read_line() {
            Ok(Some(line)) => {
                if out.write_all(line.bytes).and_then(|_| out.write_all(b"\n")).is_err() {
                    return runtime_error("failed to write output");
                }
            }
            Ok(None) => break,
            Err(err) => return runtime_error(&err.to_string()),
        }
    }
    0
}

fn run_sample(args: &[String]) -> i32 {
    let mut n: usize = 10;
    let mut seed: Option<u64> = None;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                let v = match arg_value(args, i, "sample", "-n") {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                n = match parse_arg("sample", "-n", v) {
                    Ok(n) => n,
                    Err(code) => return code,
                };
                i += 2;
            }
            "--seed" => {
                let v = match arg_value(args, i, "sample", "--seed") {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                seed = Some(match parse_arg("sample", "--seed", v) {
                    Ok(s) => s,
                    Err(code) => return code,
                });
                i += 2;
            }
            "-h" | "--help" => {
                println!("usage: jsonl-peek sample [-n N] [--seed S] [FILE]");
                return 0;
            }
            other if file.is_none() && !other.starts_with('-') => {
                file = Some(other.to_string());
                i += 1;
            }
            other => return usage_error("sample", &format!("unexpected argument '{other}'")),
        }
    }

    let reader = match open_input(file.as_deref()) {
        Ok(r) => r,
        Err(err) => return runtime_error(&format!("{}: {err}", file.as_deref().unwrap_or("-"))),
    };

    let mut rng = SplitMix64::new(seed.unwrap_or_else(default_seed));
    let mut reservoir: Reservoir<(u64, Vec<u8>)> = Reservoir::new(n);
    let mut lines = LineReader::new(reader);
    loop {
        match lines.read_line() {
            Ok(Some(line)) => {
                if !line.is_blank() {
                    reservoir.add((line.number, line.bytes.to_vec()), &mut rng);
                }
            }
            Ok(None) => break,
            Err(err) => return runtime_error(&err.to_string()),
        }
    }

    let mut sample = reservoir.into_vec();
    sample.sort_by_key(|(number, _)| *number);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (_, bytes) in sample {
        if out.write_all(&bytes).and_then(|_| out.write_all(b"\n")).is_err() {
            return runtime_error("failed to write output");
        }
    }
    0
}

fn run_stats(args: &[String]) -> i32 {
    let mut fields: Vec<FieldPath> = Vec::new();
    let mut top: usize = 10;
    let mut max_errors: usize = 10;
    let mut json_output = false;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--field" => {
                let v = match arg_value(args, i, "stats", "--field") {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                match FieldPath::parse(v) {
                    Ok(path) => fields.push(path),
                    Err(err) => return usage_error("stats", &format!("invalid --field '{v}': {err}")),
                }
                i += 2;
            }
            "--top" => {
                let v = match arg_value(args, i, "stats", "--top") {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                top = match parse_arg("stats", "--top", v) {
                    Ok(n) => n,
                    Err(code) => return code,
                };
                i += 2;
            }
            "--max-errors" => {
                let v = match arg_value(args, i, "stats", "--max-errors") {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                max_errors = match parse_arg("stats", "--max-errors", v) {
                    Ok(n) => n,
                    Err(code) => return code,
                };
                i += 2;
            }
            "--json" => {
                json_output = true;
                i += 1;
            }
            "-h" | "--help" => {
                println!("usage: jsonl-peek stats [--field PATH]... [--top N] [--max-errors N] [--json] [FILE]");
                return 0;
            }
            other if file.is_none() && !other.starts_with('-') => {
                file = Some(other.to_string());
                i += 1;
            }
            other => return usage_error("stats", &format!("unexpected argument '{other}'")),
        }
    }

    let reader = match open_input(file.as_deref()) {
        Ok(r) => r,
        Err(err) => return runtime_error(&format!("{}: {err}", file.as_deref().unwrap_or("-"))),
    };

    let stats = match Stats::from_reader(reader, StatsOptions { fields, max_errors }) {
        Ok(s) => s,
        Err(err) => return runtime_error(&err.to_string()),
    };

    if json_output {
        println!("{}", json_stats(&stats, top));
    } else {
        print_stats_human(&stats, top, file.as_deref());
    }
    0
}

fn run_schema(args: &[String]) -> i32 {
    let mut depth: usize = 3;
    let mut min_rate: f64 = 0.0;
    let mut json_output = false;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--depth" => {
                let v = match arg_value(args, i, "schema", "--depth") {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                depth = match parse_arg("schema", "--depth", v) {
                    Ok(n) => n,
                    Err(code) => return code,
                };
                i += 2;
            }
            "--min-rate" => {
                let v = match arg_value(args, i, "schema", "--min-rate") {
                    Ok(v) => v,
                    Err(code) => return code,
                };
                min_rate = match parse_arg("schema", "--min-rate", v) {
                    Ok(n) => n,
                    Err(code) => return code,
                };
                i += 2;
            }
            "--json" => {
                json_output = true;
                i += 1;
            }
            "-h" | "--help" => {
                println!("usage: jsonl-peek schema [--depth N] [--min-rate R] [--json] [FILE]");
                return 0;
            }
            other if file.is_none() && !other.starts_with('-') => {
                file = Some(other.to_string());
                i += 1;
            }
            other => return usage_error("schema", &format!("unexpected argument '{other}'")),
        }
    }

    let reader = match open_input(file.as_deref()) {
        Ok(r) => r,
        Err(err) => return runtime_error(&format!("{}: {err}", file.as_deref().unwrap_or("-"))),
    };

    let schema = match Schema::from_reader(reader, SchemaOptions { depth, min_rate }) {
        Ok(s) => s,
        Err(err) => return runtime_error(&err.to_string()),
    };

    if json_output {
        println!("{}", json_schema(&schema));
    } else {
        print_schema_human(&schema, depth, file.as_deref());
    }
    0
}

fn open_input(path: Option<&str>) -> io::Result<Box<dyn BufRead>> {
    match path {
        None | Some("-") => Ok(Box::new(io::BufReader::new(io::stdin()))),
        Some(p) => Ok(Box::new(io::BufReader::new(fs::File::open(p)?))),
    }
}

fn arg_value<'a>(args: &'a [String], i: usize, command: &str, flag: &str) -> Result<&'a str, i32> {
    args.get(i + 1)
        .map(|s| s.as_str())
        .ok_or_else(|| usage_error(command, &format!("{flag} requires a value")))
}

fn parse_arg<T: std::str::FromStr>(command: &str, flag: &str, value: &str) -> Result<T, i32> {
    value
        .parse::<T>()
        .map_err(|_| usage_error(command, &format!("invalid value for {flag}: '{value}'")))
}

fn usage_error(command: &str, message: &str) -> i32 {
    eprintln!("jsonl-peek {command}: {message}");
    2
}

fn runtime_error(message: &str) -> i32 {
    eprintln!("jsonl-peek: {message}");
    1
}

/// A default seed for `sample` when `--seed` is not given.
///
/// Not reproducible by design - reproducibility is what `--seed` is for.
fn default_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn format_bytes(n: u64) -> String {
    format!("{}   ({:.1} KiB)", format_count(n), n as f64 / 1024.0)
}

fn percent(n: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        n as f64 / total as f64 * 100.0
    }
}

fn opt_u64(v: Option<u64>) -> String {
    v.map(format_count).unwrap_or_else(|| "-".to_string())
}

fn opt_f64(v: Option<f64>) -> String {
    v.map(|f| format!("{f:.1}")).unwrap_or_else(|| "-".to_string())
}

fn format_type_counts(types: &TypeCounts) -> String {
    types
        .iter()
        .map(|(name, count)| format!("{name}:{}", format_count(count)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_stats_human(stats: &Stats, top: usize, file: Option<&str>) {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let _ = writeln!(out, "file    {}", file.unwrap_or("-"));
    let _ = writeln!(
        out,
        "lines  {}   blank {}   invalid {}   valid {}",
        format_count(stats.lines),
        format_count(stats.blank),
        format_count(stats.invalid()),
        format_count(stats.valid)
    );
    let _ = writeln!(out, "bytes  {}", format_bytes(stats.bytes));
    let _ = writeln!(out, "top level  {}", format_type_counts(&stats.top_level_types));
    let _ = writeln!(out);

    let hist = &stats.line_length;
    let _ = writeln!(out, "line length in bytes");
    let _ = writeln!(
        out,
        "  min {}   p50 {}   p90 {}   p99 {}   max {}   mean {}",
        opt_u64(hist.min()),
        opt_u64(hist.percentile(0.5)),
        opt_u64(hist.percentile(0.9)),
        opt_u64(hist.percentile(0.99)),
        opt_u64(hist.max()),
        opt_f64(hist.mean())
    );
    let _ = writeln!(out);

    let keys = stats.keys();
    let _ = writeln!(out, "top level keys over {} objects", format_count(stats.valid));
    for key in &keys {
        let _ = writeln!(
            out,
            "  {:<24} {:>10}  {:>5.1}%  {}",
            key.key,
            format_count(key.count),
            percent(key.count, stats.valid),
            format_type_counts(&key.types)
        );
    }

    for field in &stats.fields {
        let _ = writeln!(out);
        let _ = writeln!(out, "field {}", field.path);
        let _ = writeln!(
            out,
            "  present in {} of {} records ({:.1}%), {} values, types {}",
            format_count(field.present),
            format_count(stats.valid),
            percent(field.present, stats.valid),
            format_count(field.values),
            format_type_counts(&field.types)
        );
        let _ = writeln!(
            out,
            "  {} distinct values{}",
            field.distinct(),
            if field.values_capped { " (capped)" } else { "" }
        );
        for (value, count) in field.top(top) {
            let _ = writeln!(out, "  {:>10}  {:>5.1}%  {value}", format_count(count), percent(count, field.values));
        }
    }

    let total_issues = stats.issues.len() as u64 + stats.issues_truncated;
    if total_issues > 0 {
        let _ = writeln!(out);
        let _ = writeln!(out, "invalid lines ({} total, showing {})", total_issues, stats.issues.len());
        for issue in &stats.issues {
            let _ = writeln!(out, "  line {} col {}: {}", issue.line, issue.column, issue.reason);
        }
    }
}

fn print_schema_human(schema: &Schema, depth: usize, file: Option<&str>) {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let _ = writeln!(out, "file    {}", file.unwrap_or("-"));
    let _ = writeln!(out, "{} records, depth {}", format_count(schema.records), depth);
    let _ = writeln!(out);
    for path in schema.paths() {
        let _ = writeln!(
            out,
            "  {:<40} {:>6.1}%  {}",
            path.path,
            schema.rate(path) * 100.0,
            format_type_counts(&path.types)
        );
    }
    let _ = writeln!(out);
    if schema.unparseable > 0 {
        let noun = if schema.unparseable == 1 { "line" } else { "lines" };
        let _ = writeln!(out, "{} unparseable {noun} skipped", schema.unparseable);
    }
    if schema.paths_capped {
        let _ = writeln!(out, "path table capped at {MAX_PATHS}");
    }
}

fn json_type_counts(types: &TypeCounts, out: &mut String) {
    out.push('{');
    for (i, (name, count)) in types.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{name}\":{count}"));
    }
    out.push('}');
}

fn json_write_opt_u64(out: &mut String, key: &str, value: Option<u64>) {
    out.push_str(&format!(",\"{key}\":"));
    match value {
        Some(v) => out.push_str(&v.to_string()),
        None => out.push_str("null"),
    }
}

fn json_histogram(hist: &Histogram, out: &mut String) {
    out.push_str(&format!("{{\"count\":{}", hist.count()));
    json_write_opt_u64(out, "min", hist.min());
    json_write_opt_u64(out, "p50", hist.percentile(0.5));
    json_write_opt_u64(out, "p90", hist.percentile(0.9));
    json_write_opt_u64(out, "p99", hist.percentile(0.99));
    json_write_opt_u64(out, "max", hist.max());
    out.push_str(",\"mean\":");
    match hist.mean() {
        Some(m) => out.push_str(&format!("{m:.4}")),
        None => out.push_str("null"),
    }
    out.push('}');
}

fn json_field_stats(field: &jsonl_peek::stats::FieldStats, top: usize, out: &mut String) {
    out.push('{');
    out.push_str("\"path\":");
    json::escape_into(&field.path.to_string(), out);
    out.push_str(&format!(
        ",\"present\":{},\"values\":{},\"distinct\":{},\"values_capped\":{},",
        field.present,
        field.values,
        field.distinct(),
        field.values_capped
    ));
    out.push_str("\"types\":");
    json_type_counts(&field.types, out);
    out.push_str(",\"top\":[");
    for (i, (value, count)) in field.top(top).into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"value\":{value},\"count\":{count}}}"));
    }
    out.push_str("]}");
}

fn json_stats(stats: &Stats, top: usize) -> String {
    let mut out = String::new();
    out.push('{');
    out.push_str(&format!(
        "\"lines\":{},\"blank\":{},\"valid\":{},\"invalid\":{},\"bytes\":{},",
        stats.lines,
        stats.blank,
        stats.valid,
        stats.invalid(),
        stats.bytes
    ));
    out.push_str("\"top_level_types\":");
    json_type_counts(&stats.top_level_types, &mut out);
    out.push_str(",\"line_length\":");
    json_histogram(&stats.line_length, &mut out);
    out.push_str(&format!(",\"keys_capped\":{},", stats.keys_capped));
    out.push_str("\"keys\":[");
    for (i, key) in stats.keys().into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"key\":");
        json::escape_into(&key.key, &mut out);
        out.push_str(&format!(",\"count\":{},\"types\":", key.count));
        json_type_counts(&key.types, &mut out);
        out.push('}');
    }
    out.push_str(&format!("],\"issues_truncated\":{},", stats.issues_truncated));
    out.push_str("\"issues\":[");
    for (i, issue) in stats.issues.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"line\":{},\"column\":{},\"reason\":", issue.line, issue.column));
        json::escape_into(&issue.reason, &mut out);
        out.push('}');
    }
    out.push_str("],\"fields\":[");
    for (i, field) in stats.fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_field_stats(field, top, &mut out);
    }
    out.push_str("]}");
    out
}

fn json_schema(schema: &Schema) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{{\"records\":{},\"unparseable\":{},\"paths_capped\":{},\"paths\":[",
        schema.records, schema.unparseable, schema.paths_capped
    ));
    for (i, path) in schema.paths().into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"path\":");
        json::escape_into(&path.path, &mut out);
        out.push_str(&format!(
            ",\"present\":{},\"occurrences\":{},\"rate\":{:.4},\"types\":",
            path.present,
            path.occurrences,
            schema.rate(path)
        ));
        json_type_counts(&path.types, &mut out);
        out.push('}');
    }
    out.push_str("]}");
    out
}
