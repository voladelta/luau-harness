//! Smart printer: values become truncated summaries instead of dumps.
use mlua::{Table, Value};

const STR_TOP: usize = 6000; // a bare string the model asked to see
const STR_CELL: usize = 200; // a string nested inside a table
const ROWS: usize = 5; // record rows shown under a table summary
const ITEMS: usize = 10; // scalars / keys shown inline
const DEPTH: usize = 3;
const ROW_CHARS: usize = 240;

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…(+{} chars)", s.chars().count() - max)
}

fn num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Splits a table into its array part (1..n) and the remaining keyed entries.
fn split(t: &Table) -> (Vec<Value>, Vec<(String, Value)>) {
    let n = t.raw_len();
    let arr: Vec<Value> = (1..=n).filter_map(|i| t.raw_get(i).ok()).collect();
    let mut rest = Vec::new();
    for (k, v) in t.pairs::<Value, Value>().flatten() {
        let in_arr = match &k {
            Value::Integer(i) => *i >= 1 && (*i as usize) <= n,
            Value::Number(f) => f.fract() == 0.0 && *f >= 1.0 && (*f as usize) <= n,
            _ => false,
        };
        if !in_arr {
            let key = match &k {
                Value::String(s) => s.to_string_lossy().to_string(),
                Value::Integer(i) => i.to_string(),
                Value::Number(f) => num(*f),
                other => format!("<{}>", other.type_name()),
            };
            rest.push((key, v));
        }
    }
    rest.sort_by(|a, b| a.0.cmp(&b.0));
    (arr, rest)
}

fn keyed(t: &Table) -> Option<Vec<String>> {
    let (arr, rest) = split(t);
    if arr.is_empty() && !rest.is_empty() {
        Some(rest.into_iter().map(|(k, _)| k).collect())
    } else {
        None
    }
}

fn is_records(arr: &[Value]) -> bool {
    !arr.is_empty()
        && arr.iter().take(ROWS).all(|v| matches!(v, Value::Table(t) if keyed(t).is_some()))
}

fn fmt(v: &Value, depth: usize) -> String {
    match v {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => num(*n),
        Value::String(s) => {
            let s = s.to_string_lossy();
            format!("\"{}\"", clip(&s.replace('\n', "\\n"), STR_CELL))
        }
        Value::Table(t) => fmt_table(t, depth),
        Value::Function(_) => "<function>".into(),
        other => format!("<{}>", other.type_name()),
    }
}

fn fmt_table(t: &Table, depth: usize) -> String {
    let (arr, rest) = split(t);
    if arr.is_empty() && rest.is_empty() {
        return "{}".into();
    }
    if depth >= DEPTH {
        return format!("<table: {} items>", arr.len() + rest.len());
    }
    if is_records(&arr) {
        let keys = match &arr[0] {
            Value::Table(r) => keyed(r).unwrap_or_default().join(","),
            _ => String::new(),
        };
        return format!("<table: {} rows {{{keys}}}>", arr.len());
    }
    let mut parts: Vec<String> = arr.iter().take(ITEMS).map(|v| fmt(v, depth + 1)).collect();
    let mut shown = parts.len();
    for (k, v) in rest.iter().take(ITEMS.saturating_sub(shown)) {
        parts.push(format!("{k}={}", fmt(v, depth + 1)));
        shown += 1;
    }
    let total = arr.len() + rest.len();
    if total > shown {
        parts.push(format!("…+{}", total - shown));
    }
    if rest.is_empty() {
        format!("[{}]", parts.join(", "))
    } else {
        format!("{{{}}}", parts.join(", "))
    }
}

/// Summary of a single top-level value (what `print` and the last expression show).
pub fn top(v: &Value) -> String {
    match v {
        Value::String(s) => {
            let s = s.to_string_lossy();
            let n = s.chars().count();
            if n <= STR_TOP {
                s.to_string()
            } else {
                let head: String = s.chars().take(STR_TOP).collect();
                format!("{head}\n…[string truncated: {} of {n} chars shown; slice it or use read(path,{{offset,limit}})]", STR_TOP)
            }
        }
        Value::Table(t) => top_table(t),
        other => fmt(other, 0),
    }
}

fn top_table(t: &Table) -> String {
    let (arr, rest) = split(t);
    let capped = t.get::<bool>("truncated").unwrap_or(false);
    let cap = if capped { ", capped" } else { "" };
    if is_records(&arr) {
        let keys = match &arr[0] {
            Value::Table(r) => keyed(r).unwrap_or_default().join(","),
            _ => String::new(),
        };
        let mut out = format!("<table: {} rows {{{keys}}}{cap}>", arr.len());
        for (i, row) in arr.iter().take(ROWS).enumerate() {
            out.push_str(&format!("\n  [{}] {}", i + 1, clip(&fmt(row, 1), ROW_CHARS)));
        }
        if arr.len() > ROWS {
            out.push_str(&format!("\n  …+{} more rows", arr.len() - ROWS));
        }
        return out;
    }
    if !arr.is_empty() && rest.is_empty() {
        return format!("<array: {} items{cap}> {}", arr.len(), fmt_table(t, 0));
    }
    if arr.is_empty() && !rest.is_empty() {
        return format!("<table: {} keys> {}", rest.len(), fmt_table(t, 0));
    }
    fmt_table(t, 0)
}
