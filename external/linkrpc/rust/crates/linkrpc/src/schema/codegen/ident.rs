//! Identifier casing & sanitization helpers for the Rust code generator.
//!
//! All conversions are pure and deterministic: the same wire string always maps
//! to the same Rust identifier, so generated output is stable.

/// Rust 2021 reserved words (and a few reserved-for-future) that cannot be used
/// as bare identifiers. Field/variant names colliding with these are escaped.
const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while", "async", "await", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "union",
];

fn is_keyword(s: &str) -> bool {
    RUST_KEYWORDS.contains(&s)
}

/// Split an arbitrary wire name into lowercase word tokens, treating runs of
/// non-alphanumeric characters and camelCase/PascalCase boundaries as splits.
fn words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower_or_digit = false;
    for ch in input.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && prev_lower_or_digit && !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            cur.extend(ch.to_lowercase());
            prev_lower_or_digit = ch.is_lowercase() || ch.is_numeric();
        } else {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower_or_digit = false;
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Convert a wire name to `PascalCase` for a Rust type or enum-variant name.
///
/// Keyword collisions (e.g. the wire value `"self"` mapping to the reserved
/// word `Self`) are escaped with a trailing underscore. A raw-identifier
/// (`r#`) prefix is deliberately *not* used here: `Self`/`crate`/`self`/`super`
/// cannot be raw identifiers at all, and a trailing underscore keeps the result
/// safe to concatenate into synthetic type names.
pub fn to_pascal_case(input: &str) -> String {
    let mut out = String::new();
    for word in words(input) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() {
        out.push('X');
    }
    if out.chars().next().is_some_and(|c| c.is_numeric()) {
        out.insert(0, '_');
    }
    if is_keyword(&out) {
        out.push('_');
    }
    out
}

/// Convert a wire name to `snake_case` for a Rust field or method name, escaping
/// Rust keywords with a raw-identifier (`r#`) prefix where legal, else a
/// trailing underscore.
pub fn to_snake_case(input: &str) -> String {
    let mut out = String::new();
    for (i, word) in words(input).iter().enumerate() {
        if i != 0 {
            out.push('_');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        out.push('_');
    }
    if out.chars().next().is_some_and(|c| c.is_numeric()) {
        out.insert(0, '_');
    }
    escape_ident(out)
}

/// Escape a candidate identifier that collides with a Rust keyword.
fn escape_ident(name: String) -> String {
    if !is_keyword(&name) {
        return name;
    }
    // A handful of keywords are not valid as raw identifiers; fall back to a
    // trailing underscore for those.
    match name.as_str() {
        "crate" | "self" | "Self" | "super" => format!("{name}_"),
        _ => format!("r#{name}"),
    }
}

/// The wire form of a raw (possibly `r#`-escaped) Rust identifier, for serde
/// `rename` comparison.
pub fn strip_raw(ident: &str) -> &str {
    ident.strip_prefix("r#").unwrap_or(ident)
}
