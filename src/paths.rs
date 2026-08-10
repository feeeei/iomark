//! Turning canonicalized paths back into something worth showing a user.
//!
//! `Path::canonicalize()` on Windows returns the verbatim form — `\\?\D:\dir`
//! rather than `D:\dir`. That prefix is an instruction to the loader to skip
//! path parsing, not part of the name: nothing else in the system speaks it.
//! It reached the header as `disk 新加卷 (\\?\D:, NTFS)` because
//! `GetVolumePathNameW()` hands a verbatim path's volume root back verbatim
//! too, and it reached fio as a failure (see `fio_path` in src/fio.rs).

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// The longest path Windows accepts without the verbatim prefix.
const MAX_PATH: usize = 260;

/// The ordinary form of a canonicalized path, where one exists.
///
/// Keeps the verbatim prefix when dropping it would change the meaning: a path
/// at or past `MAX_PATH` is only reachable through it, and a volume GUID path
/// (`\\?\Volume{…}\`) has no plain spelling at all.
pub fn simplify(path: &Path) -> PathBuf {
    // A Unix directory may legitimately be named `\\?\x`; leave it alone.
    if !cfg!(windows) {
        return path.to_owned();
    }
    match strip_verbatim(path) {
        Cow::Owned(plain) if plain.encode_utf16().count() < MAX_PATH => PathBuf::from(plain),
        _ => path.to_owned(),
    }
}

/// Drops a Windows verbatim prefix, borrowing when there is nothing to drop.
///
/// Unlike [`simplify`], this makes no judgement about whether the result is
/// still usable — fio cannot parse the verbatim form under any circumstances,
/// so it takes the plain one regardless of length.
pub fn strip_verbatim(path: &Path) -> Cow<'_, str> {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return Cow::Owned(format!(r"\\{rest}"));
    }
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return text;
    };
    // Only a drive-letter path means the same thing without the prefix.
    let drive = {
        let mut c = rest.chars();
        matches!((c.next(), c.next()), (Some(a), Some(':')) if a.is_ascii_alphabetic())
    };
    if drive {
        Cow::Owned(rest.to_owned())
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_verbatim_prefix_from_drive_and_unc_paths() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\D:\iomark")), r"D:\iomark");
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\dir")),
            r"\\server\share\dir"
        );
    }

    #[test]
    fn leaves_paths_without_a_plain_form_alone() {
        // A volume GUID path is only addressable through the prefix.
        let guid = r"\\?\Volume{9f4a1b2c-0000-0000-0000-000000000000}\dir";
        assert_eq!(strip_verbatim(Path::new(guid)), guid);
        assert_eq!(strip_verbatim(Path::new(r"C:\dir")), r"C:\dir");
        assert_eq!(strip_verbatim(Path::new("/tmp/dir")), "/tmp/dir");
    }

    #[test]
    fn simplify_keeps_the_prefix_a_long_path_needs() {
        let long = format!(r"\\?\D:\{}", "a".repeat(MAX_PATH));
        assert_eq!(simplify(Path::new(&long)), PathBuf::from(&long));
    }

    #[cfg(windows)]
    #[test]
    fn simplify_drops_the_prefix_from_an_ordinary_path() {
        assert_eq!(
            simplify(Path::new(r"\\?\D:\iomark")),
            PathBuf::from(r"D:\iomark")
        );
    }
}
