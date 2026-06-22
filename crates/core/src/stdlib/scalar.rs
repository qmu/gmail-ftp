//! Pure **scalar** built-ins (RFD-0001 §3, ticket t08): string, path, date, number,
//! `COALESCE`, and `IF`. Each is a `fn(&[Value], &EvalCtx) -> Result<Value, FnError>`
//! that maps argument values to a result value with **no I/O** (the purity invariant).
//!
//! Argument type checks are explicit per-function: an ill-typed argument yields a
//! structured [`FnError::Type`] (carrying only type *labels*, never the value), and a
//! domain miss (`SUBSTR` start of 0, malformed date) yields [`FnError::Domain`]. `Null`
//! propagation follows SQL semantics: a `Null` argument to a scalar yields `Null` (except
//! `COALESCE`/`IF`, which inspect nulls deliberately).

use cfs_types::Value;

use super::{value_type_label, BuiltinFn, EvalCtx, FnError, FnSig};
use cfs_types::ColumnType;

/// The full set of scalar built-ins, in stable (name) order.
pub(super) fn scalar_builtins() -> Vec<BuiltinFn> {
    vec![
        // --- string ---
        BuiltinFn::scalar("UPPER", FnSig::fixed(1, ColumnType::Text), upper),
        BuiltinFn::scalar("LOWER", FnSig::fixed(1, ColumnType::Text), lower),
        BuiltinFn::scalar("TRIM", FnSig::fixed(1, ColumnType::Text), trim),
        BuiltinFn::scalar("LENGTH", FnSig::fixed(1, ColumnType::Int), length),
        BuiltinFn::scalar("SUBSTR", FnSig::range(2, 3, ColumnType::Text), substr),
        BuiltinFn::scalar("REPLACE", FnSig::fixed(3, ColumnType::Text), replace),
        BuiltinFn::scalar(
            "SPLIT",
            FnSig::fixed(2, ColumnType::Array(Box::new(ColumnType::Text))),
            split,
        ),
        BuiltinFn::scalar("CONCAT", FnSig::variadic(0, ColumnType::Text), concat),
        BuiltinFn::scalar("LIKE", FnSig::fixed(2, ColumnType::Bool), like),
        // --- path ---
        BuiltinFn::scalar("BASENAME", FnSig::fixed(1, ColumnType::Text), basename),
        BuiltinFn::scalar("DIRNAME", FnSig::fixed(1, ColumnType::Text), dirname),
        BuiltinFn::scalar("EXT", FnSig::fixed(1, ColumnType::Text), ext),
        // --- date ---
        BuiltinFn::scalar("DATE", FnSig::fixed(1, ColumnType::Date), date),
        BuiltinFn::scalar("PARSE_DATE", FnSig::fixed(1, ColumnType::Date), parse_date),
        BuiltinFn::scalar(
            "FORMAT_DATE",
            FnSig::fixed(1, ColumnType::Text),
            format_date,
        ),
        BuiltinFn::scalar("DATE_ADD", FnSig::fixed(2, ColumnType::Date), date_add),
        BuiltinFn::scalar("DATE_DIFF", FnSig::fixed(2, ColumnType::Int), date_diff),
        // --- number ---
        BuiltinFn::scalar("ABS", FnSig::fixed(1, ColumnType::Float), abs),
        BuiltinFn::scalar("ROUND", FnSig::fixed(1, ColumnType::Int), round),
        BuiltinFn::scalar("FLOOR", FnSig::fixed(1, ColumnType::Int), floor),
        BuiltinFn::scalar("CEIL", FnSig::fixed(1, ColumnType::Int), ceil),
        BuiltinFn::scalar("INT", FnSig::fixed(1, ColumnType::Int), to_int),
        BuiltinFn::scalar("FLOAT", FnSig::fixed(1, ColumnType::Float), to_float),
        BuiltinFn::scalar("TEXT", FnSig::fixed(1, ColumnType::Text), to_text),
        // --- conditional ---
        BuiltinFn::scalar(
            "COALESCE",
            FnSig::variadic(1, ColumnType::Unknown),
            coalesce,
        ),
        BuiltinFn::scalar("IF", FnSig::fixed(3, ColumnType::Unknown), if_),
    ]
}

/// Extract a `&str` argument or raise a structured type error (`Null` is the caller's
/// concern — most scalars null-propagate before reaching this).
fn as_text<'a>(name: &str, v: &'a Value) -> Result<&'a str, FnError> {
    match v {
        Value::Text(s) => Ok(s.as_str()),
        other => Err(FnError::Type {
            name: name.to_string(),
            expected: "Text",
            found: value_type_label(other),
        }),
    }
}

/// Extract an integer argument (accepts `Int`/`Timestamp`), or a type error.
fn as_int(name: &str, v: &Value) -> Result<i64, FnError> {
    match v {
        Value::Int(n) | Value::Timestamp(n) => Ok(*n),
        other => Err(FnError::Type {
            name: name.to_string(),
            expected: "Int",
            found: value_type_label(other),
        }),
    }
}

/// Whether the single argument is `Null` (for SQL-style null propagation).
fn is_null(args: &[Value]) -> bool {
    matches!(args.first(), Some(Value::Null))
}

// ---- string ----

fn upper(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    Ok(Value::Text(as_text("UPPER", &args[0])?.to_uppercase()))
}

fn lower(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    Ok(Value::Text(as_text("LOWER", &args[0])?.to_lowercase()))
}

fn trim(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    Ok(Value::Text(as_text("TRIM", &args[0])?.trim().to_string()))
}

/// `LENGTH` is the **Unicode scalar count** (not bytes) — so `'café'` is 4, not 5.
fn length(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    Ok(Value::Int(
        as_text("LENGTH", &args[0])?.chars().count() as i64
    ))
}

/// `SUBSTR(s, start[, len])` — **1-based**, char-indexed (Unicode-safe). `start` must be
/// `>= 1`; `len` (if given) must be `>= 0`. Out-of-range slices clamp to the available
/// tail rather than erroring (SQL substring semantics).
fn substr(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    let s = as_text("SUBSTR", &args[0])?;
    let start = as_int("SUBSTR", &args[1])?;
    if start < 1 {
        return Err(FnError::Domain {
            name: "SUBSTR".to_string(),
            reason: "start_must_be_one_based",
        });
    }
    let chars: Vec<char> = s.chars().collect();
    let begin = (start as usize) - 1;
    let end = match args.get(2) {
        Some(Value::Null) => return Ok(Value::Null),
        Some(len_v) => {
            let len = as_int("SUBSTR", len_v)?;
            if len < 0 {
                return Err(FnError::Domain {
                    name: "SUBSTR".to_string(),
                    reason: "length_must_be_non_negative",
                });
            }
            begin.saturating_add(len as usize).min(chars.len())
        }
        None => chars.len(),
    };
    let begin = begin.min(chars.len());
    Ok(Value::Text(chars[begin..end].iter().collect()))
}

fn replace(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    let s = as_text("REPLACE", &args[0])?;
    let from = as_text("REPLACE", &args[1])?;
    let to = as_text("REPLACE", &args[2])?;
    // An empty `from` would loop infinitely in some impls; SQL leaves the string intact.
    if from.is_empty() {
        return Ok(Value::Text(s.to_string()));
    }
    Ok(Value::Text(s.replace(from, to)))
}

fn split(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    let s = as_text("SPLIT", &args[0])?;
    let sep = as_text("SPLIT", &args[1])?;
    let parts: Vec<Value> = if sep.is_empty() {
        s.chars().map(|c| Value::Text(c.to_string())).collect()
    } else {
        s.split(sep).map(|p| Value::Text(p.to_string())).collect()
    };
    Ok(Value::Array(parts))
}

/// `CONCAT(a, b, …)` — string concatenation; a `Null` argument contributes the empty
/// string (SQL `CONCAT` semantics, distinct from `||`). Non-text args are coerced via the
/// same lexical form `TEXT` uses.
fn concat(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    let mut out = String::new();
    for a in args {
        match a {
            Value::Null => {}
            Value::Text(s) => out.push_str(s),
            other => out.push_str(&lexical_text(other)),
        }
    }
    Ok(Value::Text(out))
}

/// `LIKE(s, pattern)` — SQL `LIKE` with `%` (any run) and `_` (any single char). A helper
/// form of the `LIKE` operator, usable in expression position.
fn like(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) || matches!(args.get(1), Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let s = as_text("LIKE", &args[0])?;
    let pat = as_text("LIKE", &args[1])?;
    Ok(Value::Bool(like_match(s, pat)))
}

/// SQL `LIKE` matcher (`%` = any run incl. empty, `_` = exactly one char), Unicode-safe.
fn like_match(s: &str, pat: &str) -> bool {
    let text: Vec<char> = s.chars().collect();
    let pattern: Vec<char> = pat.chars().collect();
    like_rec(&text, &pattern)
}

fn like_rec(text: &[char], pat: &[char]) -> bool {
    match pat.split_first() {
        None => text.is_empty(),
        Some((&'%', rest)) => {
            // `%` matches zero or more chars: try every suffix of `text`.
            (0..=text.len()).any(|i| like_rec(&text[i..], rest))
        }
        Some((&'_', rest)) => !text.is_empty() && like_rec(&text[1..], rest),
        Some((&c, rest)) => text.first() == Some(&c) && like_rec(&text[1..], rest),
    }
}

// ---- path ----

/// `BASENAME('/a/b/c.txt') = 'c.txt'` — the final path component (no trailing slash).
fn basename(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    let p = as_text("BASENAME", &args[0])?.trim_end_matches('/');
    let base = p.rsplit('/').next().unwrap_or(p);
    Ok(Value::Text(base.to_string()))
}

/// `DIRNAME('/a/b/c.txt') = '/a/b'` — the parent path (`.` when there is no parent).
fn dirname(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    let p = as_text("DIRNAME", &args[0])?.trim_end_matches('/');
    match p.rsplit_once('/') {
        Some(("", _)) => Ok(Value::Text("/".to_string())),
        Some((dir, _)) => Ok(Value::Text(dir.to_string())),
        None => Ok(Value::Text(".".to_string())),
    }
}

/// `EXT('c.txt') = 'txt'` — the extension after the final `.` of the basename, or `''`
/// when there is none (a leading-dot file like `.gitignore` has no extension).
fn ext(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    let p = as_text("EXT", &args[0])?.trim_end_matches('/');
    let base = p.rsplit('/').next().unwrap_or(p);
    match base.rsplit_once('.') {
        Some(("", _)) => Ok(Value::Text(String::new())),
        Some((_, e)) => Ok(Value::Text(e.to_string())),
        None => Ok(Value::Text(String::new())),
    }
}

// ---- date (ISO-8601 `YYYY-MM-DD`, stored as epoch-days `Int`) ----

/// `DATE('2026-06-22')` — parse an ISO date string into an epoch-day `Int`. Same as
/// `PARSE_DATE` with the canonical format.
fn date(args: &[Value], ctx: &EvalCtx) -> Result<Value, FnError> {
    parse_date(args, ctx)
}

/// `PARSE_DATE('2026-06-22')` — ISO `YYYY-MM-DD` → epoch days since 1970-01-01.
fn parse_date(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    let s = as_text("PARSE_DATE", &args[0])?;
    let days = parse_iso_date(s).ok_or_else(|| FnError::Domain {
        name: "PARSE_DATE".to_string(),
        reason: "expected_iso_yyyy_mm_dd",
    })?;
    Ok(Value::Int(days))
}

/// `FORMAT_DATE(<epoch-days>)` — epoch-day `Int` → ISO `YYYY-MM-DD` text. The exact
/// inverse of `PARSE_DATE` (round-trip: `FORMAT_DATE(PARSE_DATE(s)) == s`).
fn format_date(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) {
        return Ok(Value::Null);
    }
    let days = as_int("FORMAT_DATE", &args[0])?;
    Ok(Value::Text(format_iso_date(days)))
}

/// `DATE_ADD(<epoch-days>, <n>)` — shift a date by `n` days.
fn date_add(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) || matches!(args.get(1), Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let base = as_int("DATE_ADD", &args[0])?;
    let n = as_int("DATE_ADD", &args[1])?;
    Ok(Value::Int(base.saturating_add(n)))
}

/// `DATE_DIFF(<a>, <b>)` — `a - b` in whole days.
fn date_diff(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    if is_null(args) || matches!(args.get(1), Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let a = as_int("DATE_DIFF", &args[0])?;
    let b = as_int("DATE_DIFF", &args[1])?;
    Ok(Value::Int(a.saturating_sub(b)))
}

// ---- number ----

fn abs(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    match args.first() {
        Some(Value::Null) => Ok(Value::Null),
        Some(Value::Int(n)) => Ok(Value::Int(n.saturating_abs())),
        Some(Value::Float(f)) => Ok(Value::Float(f.abs())),
        Some(other) => Err(FnError::Type {
            name: "ABS".to_string(),
            expected: "Int",
            found: value_type_label(other),
        }),
        None => Ok(Value::Null),
    }
}

/// Round a float to the nearest integer (half away from zero); an `Int` passes through.
fn round(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    match args.first() {
        Some(Value::Null) => Ok(Value::Null),
        Some(Value::Int(n)) => Ok(Value::Int(*n)),
        Some(Value::Float(f)) => Ok(Value::Int(f.round() as i64)),
        Some(other) => Err(FnError::Type {
            name: "ROUND".to_string(),
            expected: "Float",
            found: value_type_label(other),
        }),
        None => Ok(Value::Null),
    }
}

fn floor(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    match args.first() {
        Some(Value::Null) => Ok(Value::Null),
        Some(Value::Int(n)) => Ok(Value::Int(*n)),
        Some(Value::Float(f)) => Ok(Value::Int(f.floor() as i64)),
        Some(other) => Err(FnError::Type {
            name: "FLOOR".to_string(),
            expected: "Float",
            found: value_type_label(other),
        }),
        None => Ok(Value::Null),
    }
}

fn ceil(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    match args.first() {
        Some(Value::Null) => Ok(Value::Null),
        Some(Value::Int(n)) => Ok(Value::Int(*n)),
        Some(Value::Float(f)) => Ok(Value::Int(f.ceil() as i64)),
        Some(other) => Err(FnError::Type {
            name: "CEIL".to_string(),
            expected: "Float",
            found: value_type_label(other),
        }),
        None => Ok(Value::Null),
    }
}

/// `INT(x)` — cast to `Int`. Accepts `Int`/`Float`/`Bool`/numeric `Text`; a non-numeric
/// `Text` is a domain error.
fn to_int(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    match args.first() {
        Some(Value::Null) => Ok(Value::Null),
        Some(Value::Int(n)) => Ok(Value::Int(*n)),
        Some(Value::Float(f)) => Ok(Value::Int(*f as i64)),
        Some(Value::Bool(b)) => Ok(Value::Int(i64::from(*b))),
        Some(Value::Text(s)) => {
            s.trim()
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| FnError::Domain {
                    name: "INT".to_string(),
                    reason: "non_numeric_text",
                })
        }
        Some(other) => Err(FnError::Type {
            name: "INT".to_string(),
            expected: "Int",
            found: value_type_label(other),
        }),
        None => Ok(Value::Null),
    }
}

/// `FLOAT(x)` — cast to `Float`. Accepts `Int`/`Float`/numeric `Text`.
fn to_float(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    match args.first() {
        Some(Value::Null) => Ok(Value::Null),
        Some(Value::Int(n)) => Ok(Value::Float(*n as f64)),
        Some(Value::Float(f)) => Ok(Value::Float(*f)),
        Some(Value::Text(s)) => {
            s.trim()
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| FnError::Domain {
                    name: "FLOAT".to_string(),
                    reason: "non_numeric_text",
                })
        }
        Some(other) => Err(FnError::Type {
            name: "FLOAT".to_string(),
            expected: "Float",
            found: value_type_label(other),
        }),
        None => Ok(Value::Null),
    }
}

/// `TEXT(x)` — cast any scalar to its lexical text form.
fn to_text(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    match args.first() {
        Some(Value::Null) => Ok(Value::Null),
        Some(v) => Ok(Value::Text(lexical_text(v))),
        None => Ok(Value::Null),
    }
}

// ---- conditional ----

/// `COALESCE(a, b, …)` — the first non-`Null` argument, or `Null` if all are `Null`.
fn coalesce(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    for a in args {
        if !matches!(a, Value::Null) {
            return Ok(a.clone());
        }
    }
    Ok(Value::Null)
}

/// `IF(cond, then, else)` — `then` when `cond` is `Bool(true)`, else `else`. A `Null` or
/// non-boolean condition selects the `else` branch (SQL `CASE` falsiness).
fn if_(args: &[Value], _: &EvalCtx) -> Result<Value, FnError> {
    match &args[0] {
        Value::Bool(true) => Ok(args[1].clone()),
        Value::Bool(false) | Value::Null => Ok(args[2].clone()),
        other => Err(FnError::Type {
            name: "IF".to_string(),
            expected: "Bool",
            found: value_type_label(other),
        }),
    }
}

// ---- shared helpers ----

/// The lexical text form of a scalar value (used by `TEXT`/`CONCAT`). Non-scalars (struct
/// /array/json) fall back to a JSON-ish debug rendering; the scalars are canonical.
fn lexical_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) | Value::Timestamp(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bytes(_) => String::from("<bytes>"),
        Value::Json(j) => j.to_string(),
        Value::Struct(_) | Value::Array(_) => format!("{v:?}"),
        // `Value` is `#[non_exhaustive]`; a future variant degrades to its debug form.
        _ => format!("{v:?}"),
    }
}

/// Parse an ISO `YYYY-MM-DD` date into epoch days since 1970-01-01 (proleptic Gregorian),
/// or `None` if malformed / out of range. Self-contained (no chrono) to keep the stdlib
/// dependency-light and deterministic.
fn parse_iso_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    // Exactly `YYYY-MM-DD`.
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// Format epoch days since 1970-01-01 as an ISO `YYYY-MM-DD` string.
fn format_iso_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Whether `year` is a leap year (proleptic Gregorian).
fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in `month` of `year`.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from civil date to epoch (Howard Hinnant's algorithm), proleptic Gregorian.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date from epoch days (the inverse of [`days_from_civil`]).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
