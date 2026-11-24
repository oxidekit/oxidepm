//! The pure part of the filesystem rules: which paths a ruleset opens, decided from the profile
//! and the directory tree, with no Landlock call. (Landlock only allows, so "everything except the
//! daemon home" is built by listing what to allow.)

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};

/// `PT_INTERP` of an ELF64 little-endian executable: the dynamic loader it asks for. The kernel
/// opens that file for execution when it starts the program, so a ruleset that denies `EXECUTE`
/// everywhere must open it.
pub fn elf_interpreter(file: &File) -> Option<PathBuf> {
    let mut header = [0u8; 64];
    file.read_exact_at(&mut header, 0).ok()?;
    // magic, 64-bit, little endian
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return None;
    }
    let phoff = u64::from_le_bytes(header[32..40].try_into().ok()?);
    let phentsize = u64::from(u16::from_le_bytes(header[54..56].try_into().ok()?));
    let phnum = u64::from(u16::from_le_bytes(header[56..58].try_into().ok()?));
    if phentsize < 56 || phnum > 128 {
        return None;
    }
    for i in 0..phnum {
        let mut ph = [0u8; 56];
        file.read_exact_at(&mut ph, phoff + i * phentsize).ok()?;
        let kind = u32::from_le_bytes(ph[0..4].try_into().ok()?);
        if kind == 3 {
            let offset = u64::from_le_bytes(ph[8..16].try_into().ok()?);
            let size = u64::from_le_bytes(ph[32..40].try_into().ok()?);
            if size == 0 || size > 4096 {
                return None;
            }
            let mut name = vec![0u8; size as usize];
            file.read_exact_at(&mut name, offset).ok()?;
            let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
            name.truncate(end);
            use std::os::unix::ffi::OsStringExt;
            return Some(PathBuf::from(std::ffi::OsString::from_vec(name)));
        }
    }
    None
}

/// Every entry that must be allowed so that everything below `root` is readable except `avoid`
/// and what lies under it: for each directory on the way from `root` to `avoid`, all its entries
/// but the one on the way. Symbolic links are skipped (a rule on a link would follow it, possibly
/// into `avoid`, and a program that opens through a link is checked at the link's target anyway).
/// A directory that cannot be listed ends the walk there. `avoid` must lie below `root`.
pub fn allow_all_but(root: &Path, avoid: &Path) -> Vec<PathBuf> {
    let Ok(relative) = avoid.strip_prefix(root) else {
        return Vec::new();
    };
    let mut allowed = Vec::new();
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(next) = component else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&current) else {
            break;
        };
        for entry in entries.flatten() {
            if entry.file_name() == next {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            allowed.push(entry.path());
        }
        current.push(next);
    }
    allowed
}

/// What to open in `/proc` instead of all of it: the entries whose name is not a process id, and
/// `self`. (Another process's entry shows its memory maps, environment and descriptors.) `proc` is
/// `/proc`, a parameter for the sake of tests.
pub fn proc_entries(proc: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(proc) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.bytes().all(|b| b.is_ascii_digit())
                || entry.file_type().map_or(true, |t| t.is_symlink())
            {
                continue;
            }
            out.push(entry.path());
        }
    }
    out.push(proc.join("self"));
    out
}

/// The rules of `fs.read = any`: everything but `home` (the daemon home), with `/proc` narrowed.
pub fn read_any_paths(root: &Path, proc: &Path, home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for path in allow_all_but(root, home) {
        if path == proc {
            paths.extend(proc_entries(proc));
        } else {
            paths.push(path);
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in [
            "a/b/home/inner",
            "a/b/other",
            "a/sibling",
            "top",
            "proc/1234",
            "proc/sys",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("a/b/file"), b"x").unwrap();
        std::fs::write(root.join("a/b/home/secret"), b"x").unwrap();
        std::fs::write(root.join("proc/meminfo"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("a/b/home"), root.join("a/b/link-to-home")).unwrap();
        dir
    }

    fn rel(root: &Path, paths: Vec<PathBuf>) -> Vec<String> {
        let mut out: Vec<String> = paths
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().display().to_string())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn everything_but_the_home_is_allowed_and_the_home_and_links_are_not() {
        let dir = tree();
        let root = dir.path();
        let allowed = rel(root, allow_all_but(root, &root.join("a/b/home")));
        assert_eq!(
            allowed,
            ["a/b/file", "a/b/other", "a/sibling", "proc", "top"]
        );
        // nothing at or under the home, and the link that points into it is not a rule
        assert!(allowed
            .iter()
            .all(|p| !p.starts_with("a/b/home") && !p.contains("link")));
    }

    #[test]
    fn a_home_that_is_not_under_the_root_allows_nothing() {
        let dir = tree();
        assert!(allow_all_but(&dir.path().join("a"), Path::new("/elsewhere/home")).is_empty());
    }

    #[test]
    fn proc_is_narrowed_to_non_process_entries_and_self() {
        let dir = tree();
        let root = dir.path();
        let proc = root.join("proc");
        let rules = rel(root, read_any_paths(root, &proc, &root.join("a/b/home")));
        assert!(!rules.contains(&"proc".to_string()), "{rules:?}");
        assert!(
            rules.contains(&"proc/meminfo".to_string())
                && rules.contains(&"proc/sys".to_string())
                && rules.contains(&"proc/self".to_string()),
            "{rules:?}"
        );
        assert!(
            !rules.contains(&"proc/1234".to_string()),
            "another process's entry must stay closed: {rules:?}"
        );
    }

    #[test]
    fn the_interpreter_of_a_dynamic_executable_is_found_and_a_static_file_has_none() {
        let me = File::open("/proc/self/exe").unwrap();
        // the test binary is dynamically linked on this platform
        let interp = elf_interpreter(&me).expect("a dynamic executable names its loader");
        assert!(
            interp.is_absolute() && interp.to_string_lossy().contains("ld-"),
            "{interp:?}"
        );
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("text");
        std::fs::write(
            &text,
            b"not an elf file at all, just text that is long enough to read a header from it....",
        )
        .unwrap();
        assert_eq!(elf_interpreter(&File::open(&text).unwrap()), None);
        let tiny = dir.path().join("tiny");
        std::fs::write(&tiny, b"\x7fELF").unwrap();
        assert_eq!(elf_interpreter(&File::open(&tiny).unwrap()), None);
    }
}
