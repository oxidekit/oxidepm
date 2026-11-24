//! One parser for `.env` files, shared by the config loader (and, later, the
//! CLI).
//!
//! Rules:
//! - blank lines and lines starting with `#` are ignored;
//! - an optional leading `export ` is stripped;
//! - keys match `[A-Za-z_][A-Za-z0-9_]*`;
//! - `'single'` quotes are literal;
//! - `"double"` quotes understand `\n`, `\"` and `\\` (any other backslash
//!   sequence is kept as written);
//! - an unquoted value ends at an inline ` #` comment (a `#` preceded by
//!   whitespace) and is trimmed;
//! - only a comment may follow a closing quote.
//!
//! Entries are returned in file order; for a repeated key the caller should
//! let the later entry win.

use crate::error::{Error, Result};

/// Parse the text of a `.env` file into `(key, value)` pairs.
pub fn parse(content: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (idx, raw) in content.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = strip_export(line);
        let (key, rest) = line
            .split_once('=')
            .ok_or_else(|| err(lineno, "expected KEY=VALUE"))?;
        let key = key.trim();
        if !valid_key(key) {
            return Err(err(
                lineno,
                &format!("invalid key '{}': use letters, digits and underscores, not starting with a digit", key),
            ));
        }
        let value = parse_value(rest).map_err(|why| err(lineno, &why))?;
        if value.contains('\0') {
            return Err(err(lineno, "environment values must not contain NUL"));
        }
        out.push((key.to_string(), value));
    }
    Ok(out)
}

fn err(line: usize, why: &str) -> Error {
    Error::ConfigError(format!("env file line {}: {}", line, why))
}

fn strip_export(line: &str) -> &str {
    match line.strip_prefix("export") {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start(),
        _ => line,
    }
}

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `rest` is everything after the first `=`.
fn parse_value(rest: &str) -> std::result::Result<String, String> {
    let trimmed = rest.trim_start();
    match trimmed.chars().next() {
        Some('\'') => {
            let body = &trimmed[1..];
            let end = body.find('\'').ok_or("unterminated single quote")?;
            check_tail(&body[end + 1..])?;
            Ok(body[..end].to_string())
        }
        Some('"') => {
            let body = &trimmed[1..];
            let mut value = String::new();
            let mut chars = body.char_indices();
            while let Some((i, c)) = chars.next() {
                match c {
                    '"' => {
                        check_tail(&body[i + 1..])?;
                        return Ok(value);
                    }
                    '\\' => match chars.next() {
                        Some((_, 'n')) => value.push('\n'),
                        Some((_, '"')) => value.push('"'),
                        Some((_, '\\')) => value.push('\\'),
                        Some((_, other)) => {
                            value.push('\\');
                            value.push(other);
                        }
                        None => return Err("unterminated double quote".to_string()),
                    },
                    c => value.push(c),
                }
            }
            Err("unterminated double quote".to_string())
        }
        _ => Ok(strip_inline_comment(rest).trim().to_string()),
    }
}

/// Cut an unquoted value at the first `#` that follows whitespace.
fn strip_inline_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'#' && i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
            return &s[..i];
        }
    }
    s
}

/// After a closing quote only whitespace or a `#` comment may follow.
fn check_tail(tail: &str) -> std::result::Result<(), String> {
    let t = tail.trim();
    if t.is_empty() || t.starts_with('#') {
        Ok(())
    } else {
        Err(format!("unexpected text after closing quote: '{}'", t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(content: &str) -> std::collections::HashMap<String, String> {
        parse(content).unwrap().into_iter().collect()
    }

    #[test]
    fn envfile_parses_export_inline_comment_and_both_quote_styles() {
        let m = map(r#"
# a comment
export A=1 # trailing comment
B = two words   # another
C="say \"hi\"\nnext \\ done" # c
D='raw \n $x "kept"'
E=
F=#not-a-comment
G=a#b
export   H=spaced
"#);
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "two words");
        assert_eq!(m["C"], "say \"hi\"\nnext \\ done");
        assert_eq!(m["D"], r#"raw \n $x "kept""#);
        assert_eq!(m["E"], "");
        assert_eq!(m["F"], "#not-a-comment");
        assert_eq!(m["G"], "a#b");
        assert_eq!(m["H"], "spaced");
        assert!(!m.contains_key("export A"));
    }

    #[test]
    fn envfile_keeps_file_order_and_repeats() {
        let v = parse("A=1\nB=2\nA=3\n").unwrap();
        assert_eq!(
            v,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string()),
                ("A".to_string(), "3".to_string())
            ]
        );
    }

    #[test]
    fn envfile_rejects_bad_key() {
        for bad in ["1A=x", "A-B=x", "A B=x", "=x", "NOEQUALS", "export =x"] {
            let e = parse(bad).unwrap_err().to_string();
            assert!(e.contains("line 1"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn envfile_rejects_nul_in_a_value() {
        assert!(parse("A=one\0two").unwrap_err().to_string().contains("NUL"));
    }

    #[test]
    fn envfile_rejects_unterminated_quote_and_trailing_text() {
        assert!(parse("A=\"open").is_err());
        assert!(parse("A='open").is_err());
        assert!(parse("A=\"x\" y").is_err());
        assert!(parse("A='x'y").is_err());
        let e = parse("OK=1\nA=\"open").unwrap_err().to_string();
        assert!(e.contains("line 2"), "{e}");
    }
}
