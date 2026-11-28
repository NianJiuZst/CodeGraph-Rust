//! Path canonicalization helpers shared across crates.
//!
//! On Windows, `std::fs::canonicalize` returns verbatim (`\\?\`) paths such as
//! `\\?\C:\repo` or `\\?\UNC\server\share`. Many downstream consumers (`cmd.exe`,
//! MCP clients, JSON scripts) reject or fail to match verbatim prefixes.
//! [`canonicalize`] strips the verbatim prefix whenever the resulting normal
//! Win32 path has identical semantics, keeping `\\?\` only when required
//! (paths >= `MAX_PATH`, reserved DOS device names, trailing dots/spaces, etc.).

use std::io;
use std::path::{Path, PathBuf};

/// Canonicalize `path`, stripping the Windows verbatim (`\\?\`) prefix when safe.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    let p = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        Ok(strip_windows_verbatim(p))
    }
    #[cfg(not(windows))]
    {
        Ok(p)
    }
}

/// Strip a Windows verbatim prefix (`\\?\C:\...` or `\\?\UNC\...`) if the normal
/// Win32 path refers to the exact same filesystem location.
#[cfg(any(windows, test))]
fn strip_windows_verbatim(path: PathBuf) -> PathBuf {
    let Some(s) = path.to_str() else { return path };
    match strip_windows_verbatim_str(s) {
        Some(clean) => PathBuf::from(clean),
        None => path,
    }
}

#[cfg(any(windows, test))]
fn strip_windows_verbatim_str(s: &str) -> Option<String> {
    const MAX_PATH_UTF16: usize = 260;

    let (candidate, tail) = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        // Must have non-empty `server\share` after `\\?\UNC\`.
        let mut parts = rest.split('\\');
        let server = parts.next().filter(|p| !p.is_empty())?;
        let share = parts.next().filter(|p| !p.is_empty())?;
        let _ = (server, share);
        (format!(r"\\{rest}"), rest)
    } else {
        let rest = s.strip_prefix(r"\\?\")?;
        let b = rest.as_bytes();
        if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
            return None;
        }
        (rest.to_owned(), &rest[3..])
    };

    // MAX_PATH (260 UTF-16 code units) includes the terminating NUL.
    if candidate.encode_utf16().count() >= MAX_PATH_UTF16 {
        return None;
    }

    if tail.starts_with('\\') || tail.contains('/') || tail.contains(r"\\") {
        return None;
    }

    for seg in tail.split('\\') {
        if seg.is_empty() {
            // Trailing slash on a drive root (`C:\`) yields a single empty segment.
            continue;
        }
        if !is_safe_win32_component(seg) {
            return None;
        }
    }

    Some(candidate)
}

#[cfg(any(windows, test))]
fn is_safe_win32_component(seg: &str) -> bool {
    if seg == "." || seg == ".." || seg.ends_with(' ') || seg.ends_with('.') {
        return false;
    }
    if seg.bytes().any(|b| b < 0x20 || matches!(b, b'<' | b'>' | b':' | b'"' | b'|' | b'?' | b'*')) {
        return false;
    }
    // Win32 treats DOS device names as reserved even with extensions (e.g. `NUL.txt`),
    // and strips trailing spaces before the first dot (`CON .txt`).
    let stem = seg.split('.').next().unwrap_or(seg).trim_end_matches(' ');
    !is_reserved_dos_device(stem)
}

#[cfg(any(windows, test))]
fn is_reserved_dos_device(stem: &str) -> bool {
    if stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL")
        || stem.eq_ignore_ascii_case("CONIN$")
        || stem.eq_ignore_ascii_case("CONOUT$")
    {
        return true;
    }
    let mut chars = stem.chars();
    let (Some(a), Some(b), Some(c), Some(d), None) = (chars.next(), chars.next(), chars.next(), chars.next(), chars.next()) else {
        return false;
    };
    let prefix_ok = (a.eq_ignore_ascii_case(&'c') && b.eq_ignore_ascii_case(&'o') && c.eq_ignore_ascii_case(&'m'))
        || (a.eq_ignore_ascii_case(&'l') && b.eq_ignore_ascii_case(&'p') && c.eq_ignore_ascii_case(&'t'));
    prefix_ok && matches!(d, '1'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_normal_drive_and_unc_verbatim_paths() {
        assert_eq!(strip_windows_verbatim_str(r"\\?\C:\Users\alice\repo").as_deref(), Some(r"C:\Users\alice\repo"));
        assert_eq!(strip_windows_verbatim_str(r"\\?\D:\").as_deref(), Some(r"D:\"));
        assert_eq!(strip_windows_verbatim_str(r"\\?\UNC\fileserver\share\repo").as_deref(), Some(r"\\fileserver\share\repo"));
        assert_eq!(strip_windows_verbatim(PathBuf::from(r"\\?\C:\Users\alice\repo")), PathBuf::from(r"C:\Users\alice\repo"));
        assert_eq!(strip_windows_verbatim(PathBuf::from(r"\\?\C:\repo\nul")), PathBuf::from(r"\\?\C:\repo\nul"));
    }

    #[test]
    fn leaves_non_verbatim_and_special_prefixes_untouched() {
        assert_eq!(strip_windows_verbatim_str(r"C:\Users\alice\repo"), None);
        assert_eq!(strip_windows_verbatim_str("/usr/local/bin/cgr"), None);
        assert_eq!(strip_windows_verbatim_str(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\repo"), None);
        assert_eq!(strip_windows_verbatim_str(r"\\?\GLOBALROOT\Device\HarddiskVolume1\repo"), None);
        assert_eq!(strip_windows_verbatim_str(r"\\?\C:"), None);
        assert_eq!(strip_windows_verbatim_str(r"\\?\UNC\onlyserver"), None);
    }

    #[test]
    fn preserves_verbatim_when_path_exceeds_max_path() {
        let deep = format!(r"\\?\C:\{}", "a".repeat(257)); // candidate length 260 UTF-16 code units
        assert_eq!(strip_windows_verbatim_str(&deep), None);

        let just_under = format!(r"\\?\C:\{}", "a".repeat(256)); // candidate length 259
        assert!(strip_windows_verbatim_str(&just_under).is_some());
    }

    #[test]
    fn preserves_verbatim_for_reserved_or_unsafe_win32_components() {
        for p in [
            r"\\?\C:\repo\nul",
            r"\\?\C:\repo\CON.txt",
            r"\\?\C:\repo\CON .txt",
            r"\\?\C:\repo\CONIN$",
            r"\\?\C:\repo\conout$.log",
            r"\\?\C:\repo\COM1\file.rs",
            r"\\?\C:\repo\COM¹\file.rs",
            r"\\?\C:\repo\lpt9.log",
            r"\\?\C:\repo\lpt³.log",
            r"\\?\C:\repo\trailing.",
            r"\\?\C:\repo\trailing ",
            r"\\?\C:\repo\..\other",
            r"\\?\C:\repo\with/slash",
            r"\\?\C:\\double_after_root",
            r"\\?\C:\\",
            r"\\?\C:\repo\\double",
            r"\\?\C:\repo\bad:name",
        ] {
            assert_eq!(strip_windows_verbatim_str(p), None, "should preserve verbatim for {p}");
        }
        // Names that merely look similar to reserved devices are safe.
        assert_eq!(strip_windows_verbatim_str(r"\\?\C:\repo\console\null_ptr.rs").as_deref(), Some(r"C:\repo\console\null_ptr.rs"));
        assert_eq!(strip_windows_verbatim_str(r"\\?\C:\repo\com0\lpt0.rs").as_deref(), Some(r"C:\repo\com0\lpt0.rs"));
    }

    #[test]
    fn canonicalize_returns_clean_path_for_existing_dir() {
        let dir = std::env::temp_dir();
        let canon = canonicalize(&dir).expect("temp_dir should canonicalize");
        assert!(canon.is_absolute());
        #[cfg(windows)]
        if let Some(s) = canon.to_str() {
            if s.encode_utf16().count() < 260 {
                assert!(!s.starts_with(r"\\?\"), "unexpected verbatim prefix: {s}");
            }
        }
    }
}
