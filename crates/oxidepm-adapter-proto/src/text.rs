//! Rules for text that a publisher wrote and a terminal will print.

/// True when `s` holds a control character: C0, DEL, C1 (so ESC and CSI), or one of the Unicode
/// bidirectional formatting characters that can make a terminal show other text than was written.
/// A newline is allowed when `allow_newline` is set (descriptions only).
pub fn has_control(s: &str, allow_newline: bool) -> bool {
    s.chars().any(|c| {
        if c == '\n' && allow_newline {
            return false;
        }
        c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
    })
}

/// `^[a-z][a-z0-9-]{min-1,max-1}$` style check: first a lowercase letter, then lowercase letters,
/// digits and (when `dash`) hyphens; total length in `min..=max`.
pub fn is_name(s: &str, min: usize, max: usize, dash: bool) -> bool {
    let b = s.as_bytes();
    if b.len() < min || b.len() > max || !b[0].is_ascii_lowercase() {
        return false;
    }
    b.iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || (dash && *c == b'-'))
}

/// A bare file name of a program: no slash, no NUL, no leading '-' or '.', printable ASCII.
pub fn is_program_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with(['-', '.'])
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-'))
}

/// An environment variable name.
pub fn is_env_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// Environment keys an adapter may never set, whatever its manifest lists in `env_set`.
/// The allow-list `env_set` is the main control; this list catches a manifest that lists one anyway.
pub fn env_key_denied(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    k.starts_with("LD_")
        || k.starts_with("DYLD_")
        || k.starts_with("NPM_CONFIG_")
        || matches!(
            k.as_str(),
            "PATH"
                | "NODE_OPTIONS"
                | "NODE_PATH"
                | "BASH_ENV"
                | "ENV"
                | "IFS"
                | "SHELL"
                | "PYTHONSTARTUP"
                | "PYTHONHOME"
                | "PYTHONPATH"
                | "PERL5OPT"
                | "PERL5LIB"
                | "RUBYOPT"
                | "RUBYLIB"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "GIT_SSH"
                | "GIT_SSH_COMMAND"
                | "GIT_EXTERNAL_DIFF"
                | "GIT_ASKPASS"
                | "GIT_PROXY_COMMAND"
                | "SSH_ASKPASS"
        )
}

/// Lowercase hex of a byte slice.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// A sha256 written as 64 lowercase hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
