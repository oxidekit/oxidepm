//! `check_version`: a version string that is safe to put in a URL, and the downgrade rule.

use std::cmp::Ordering;

use crate::error::{FetchError, Result};

const MAX_LEN: usize = 64;

/// The parts of a semantic version that decide precedence (build metadata does not).
#[derive(Debug, PartialEq, Eq)]
struct Semver {
    core: [u64; 3],
    pre: Vec<String>,
}

fn numeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse(version: &str) -> std::result::Result<Semver, String> {
    let (rest, build) = match version.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (version, None),
    };
    if let Some(build) = build {
        let ok = !build.is_empty()
            && build.split('.').all(|id| {
                !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        if !ok {
            return Err("the build metadata is not valid".into());
        }
    }
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err("it is not MAJOR.MINOR.PATCH".into());
    }
    let mut nums = [0u64; 3];
    for (slot, part) in nums.iter_mut().zip(&parts) {
        if !numeric(part) || (part.len() > 1 && part.starts_with('0')) {
            return Err(format!("{part:?} is not a number without leading zeros"));
        }
        *slot = part.parse().map_err(|_| format!("{part:?} is too large"))?;
    }
    let mut pre_ids = Vec::new();
    if let Some(pre) = pre {
        for id in pre.split('.') {
            let valid =
                !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
            if !valid || (numeric(id) && id.len() > 1 && id.starts_with('0')) {
                return Err(format!("the pre-release part {id:?} is not valid"));
            }
            pre_ids.push(id.to_string());
        }
    }
    Ok(Semver {
        core: nums,
        pre: pre_ids,
    })
}

fn compare(a: &Semver, b: &Semver) -> Ordering {
    a.core
        .cmp(&b.core)
        .then_with(|| match (a.pre.is_empty(), b.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater, // a release is above its pre-releases
            (false, true) => Ordering::Less,
            (false, false) => {
                for (x, y) in a.pre.iter().zip(&b.pre) {
                    let order = match (numeric(x), numeric(y)) {
                        (true, true) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        (false, false) => x.cmp(y),
                    };
                    if order != Ordering::Equal {
                        return order;
                    }
                }
                a.pre.len().cmp(&b.pre.len())
            }
        })
}

/// `candidate` must be made of `[0-9A-Za-z.+-]` only, at most 64 characters, and be a valid
/// semantic version (no leading `v`). A version lower than `current` needs `force`. `current`
/// is the running build's version; one that is not a valid version cannot be compared and is
/// an error as well.
pub fn check_version(candidate: &str, current: &str, force: bool) -> Result<()> {
    let bad = |why: &str| {
        FetchError::BadVersion(candidate.chars().take(MAX_LEN).collect(), why.to_string())
    };
    if candidate.is_empty() || candidate.len() > MAX_LEN {
        return Err(bad("it must be 1 to 64 characters"));
    }
    if !candidate
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
    {
        return Err(bad("only letters, digits and . + - are allowed"));
    }
    let wanted = parse(candidate).map_err(|why| bad(&why))?;
    let running = parse(current).map_err(|why| {
        FetchError::BadVersion(
            current.to_string(),
            format!("the installed version cannot be compared: {why}"),
        )
    })?;
    if compare(&wanted, &running) == Ordering::Less && !force {
        return Err(FetchError::Downgrade {
            candidate: candidate.to_string(),
            current: current.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_follows_semver() {
        let order = [
            "0.9.9",
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.10.0",
            "2.0.0",
        ];
        for pair in order.windows(2) {
            let (a, b) = (parse(pair[0]).unwrap(), parse(pair[1]).unwrap());
            assert_eq!(compare(&a, &b), Ordering::Less, "{} < {}", pair[0], pair[1]);
            assert_eq!(compare(&b, &a), Ordering::Greater);
        }
        assert_eq!(
            compare(&parse("1.0.0+a").unwrap(), &parse("1.0.0+b").unwrap()),
            Ordering::Equal
        );
    }
}
