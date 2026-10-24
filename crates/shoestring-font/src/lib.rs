//! Font discovery for shoestring-wm and its helpers.
//!
//! No fontconfig: a font is a TTF/OTF path handed to [`fontdue`]. [`load`]
//! resolves one in a fixed order:
//!
//! 1. an explicit path (a config key or `--font` flag), when the caller has one;
//! 2. the caller's environment variable (`$SHOESTRING_BAR_FONT` and so on);
//! 3. [`CANDIDATES`], the usual install paths of DejaVu, Liberation and Noto on
//!    the Linux distributions and FreeBSD;
//! 4. a scan of the system and user font directories for any `.ttf`/`.otf`,
//!    best first by [`font_rank`].
//!
//! A path given in steps 1 or 2 must load: a typo there is an error, not a
//! silent fallback to another face. Steps 3 and 4 skip files that don't parse.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use fontdue::{Font, FontSettings};

/// Common install paths for a regular sans face, tried in order.
pub const CANDIDATES: &[&str] = &[
    // Debian / Ubuntu
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    // Arch / generic Linux
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    // Fedora / RHEL (the liberation package installs under
    // `liberation-sans-fonts/`, verified on Fedora 44 — the bare
    // `liberation-sans/` path below is older / other distros)
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/liberation-sans-fonts/LiberationSans-Regular.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
    // FreeBSD (pkg install dejavu / liberation-fonts-ttf / noto-basic)
    "/usr/local/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/local/share/fonts/liberation-fonts-ttf/LiberationSans-Regular.ttf",
    "/usr/local/share/fonts/noto/NotoSans-Regular.ttf",
];

/// How deep [`scan_system_fonts`] descends, and how many files it will try to
/// parse, so a large font tree can't hold up startup.
pub const SCAN_DEPTH: usize = 6;
pub const SCAN_TRIES: usize = 64;

/// Why no font could be loaded.
#[derive(Debug)]
pub enum Error {
    /// An explicitly named font (`what` says where it was named) couldn't be
    /// read or parsed.
    Named {
        what: String,
        path: PathBuf,
        reason: String,
    },
    /// Nothing in [`CANDIDATES`] or the font directories loaded.
    NotFound { env_var: Option<String> },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Named { what, path, reason } => {
                write!(f, "{what}: {}: {reason}", path.display())
            }
            Error::NotFound { env_var } => {
                write!(
                    f,
                    "no font found: none of the usual DejaVu/Liberation/Noto paths, \
                     and no .ttf/.otf under the system or user font directories"
                )?;
                if let Some(var) = env_var {
                    write!(f, "; set ${var} to a .ttf/.otf")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {}

/// Resolve a font: `explicit` (a config key or flag, described by
/// `explicit_what` in errors), then `$env_var`, then [`find_system_font`].
pub fn load(
    explicit: Option<&Path>,
    explicit_what: &str,
    env_var: Option<&str>,
) -> Result<Font, Error> {
    if let Some(path) = explicit {
        return read_named(path, explicit_what);
    }
    if let Some(var) = env_var {
        if let Some(path) = std::env::var_os(var).filter(|p| !p.is_empty()) {
            return read_named(Path::new(&path), &format!("${var}"));
        }
    }
    find_system_font()
        .map(|(_, font)| font)
        .ok_or_else(|| Error::NotFound {
            env_var: env_var.map(str::to_string),
        })
}

fn read_named(path: &Path, what: &str) -> Result<Font, Error> {
    let named = |reason: String| Error::Named {
        what: what.to_string(),
        path: path.to_path_buf(),
        reason,
    };
    let bytes = fs::read(path).map_err(|e| named(e.to_string()))?;
    parse(bytes).map_err(|e| named(format!("not a usable font ({e})")))
}

fn parse(bytes: Vec<u8>) -> Result<Font, &'static str> {
    Font::from_bytes(bytes, FontSettings::default())
}

fn read_font(path: &Path) -> Option<Font> {
    parse(fs::read(path).ok()?).ok()
}

/// The first of [`CANDIDATES`] that loads, else the best font the scan of the
/// font directories finds.
pub fn find_system_font() -> Option<(PathBuf, Font)> {
    let listed = CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find_map(|p| read_font(&p).map(|f| (p, f)));
    listed.or_else(|| {
        let found = scan_system_fonts()?;
        tracing::info!(
            path = %found.0.display(),
            "no listed font; using one found on the system"
        );
        Some(found)
    })
}

/// Font directories searched when no listed candidate exists.
pub fn system_font_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        Some(d) => dirs.push(Path::new(&d).join("fonts")),
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(Path::new(&home).join(".local/share/fonts"));
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".fonts"));
    }
    dirs
}

/// Any `.ttf`/`.otf` under [`system_font_dirs`], best first by [`font_rank`],
/// returning the first that parses.
pub fn scan_system_fonts() -> Option<(PathBuf, Font)> {
    let mut found = Vec::new();
    for dir in system_font_dirs() {
        collect_font_files(&dir, SCAN_DEPTH, &mut found);
    }
    found.sort_by(|a, b| font_rank(a).cmp(&font_rank(b)).then_with(|| a.cmp(b)));
    found
        .into_iter()
        .take(SCAN_TRIES)
        .find_map(|p| read_font(&p).map(|f| (p, f)))
}

fn collect_font_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth > 0 {
                collect_font_files(&path, depth - 1, out);
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("ttf") || e.eq_ignore_ascii_case("otf"))
        {
            out.push(path);
        }
    }
}

/// Lower is better: an upright regular sans for UI text, then other upright
/// text faces, then styled ones, and last the faces that may lack Latin
/// letters or common punctuation (the lock's password bullet, say).
pub fn font_rank(path: &Path) -> u8 {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if has(&["emoji", "symbol", "math", "icon", "awesome", "dingbat"]) {
        return 4;
    }
    let styled = has(&[
        "bold",
        "italic",
        "oblique",
        "light",
        "thin",
        "black",
        "medium",
        "semi",
        "extra",
        "demi",
        "condensed",
    ]);
    match (styled, has(&["sans"]) && !has(&["mono"])) {
        (false, true) => 0,
        (false, false) => 1,
        (true, true) => 2,
        (true, false) => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sw-font-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn font_rank_prefers_upright_sans() {
        let order = [
            "/f/NotoSans-Regular.ttf",
            "/f/NotoSerif-Regular.ttf",
            "/f/NotoSansMono-Regular.ttf",
            "/f/NotoSans-Medium.ttf",
            "/f/NotoSans-SemiBold.ttf",
            "/f/NotoSans-Bold.ttf",
            "/f/NotoSerif-Italic.ttf",
            "/f/NotoColorEmoji.ttf",
        ];
        let ranks: Vec<u8> = order.iter().map(|p| font_rank(Path::new(p))).collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{ranks:?}");
        assert_eq!(ranks[0], 0);
        assert_eq!(ranks[3], 2, "Medium is a weight, not the regular face");
        assert_eq!(ranks[7], 4);
    }

    #[test]
    fn collect_font_files_finds_ttf_and_otf_only() {
        let dir = scratch("collect");
        fs::create_dir_all(dir.join("a/b")).unwrap();
        for f in ["a/One.ttf", "a/b/Two.OTF", "a/Three.ttc", "a/readme.txt"] {
            fs::write(dir.join(f), b"").unwrap();
        }
        let mut found = Vec::new();
        collect_font_files(&dir, SCAN_DEPTH, &mut found);
        found.sort();
        assert_eq!(found, vec![dir.join("a/One.ttf"), dir.join("a/b/Two.OTF")]);
        let mut shallow = Vec::new();
        collect_font_files(&dir, 0, &mut shallow);
        assert!(shallow.is_empty(), "depth 0 must not descend: {shallow:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_named_font_that_does_not_load_is_an_error() {
        let dir = scratch("named");
        let missing = dir.join("missing.ttf");
        let err = load(Some(&missing), "--font", None).unwrap_err();
        assert!(
            matches!(&err, Error::Named { what, path, .. } if what == "--font" && *path == missing),
            "{err}"
        );

        let junk = dir.join("junk.ttf");
        fs::write(&junk, b"not a font").unwrap();
        let err = load(Some(&junk), "bar.font", None).unwrap_err();
        assert!(err.to_string().contains("not a usable font"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn not_found_names_the_override_variable() {
        let err = Error::NotFound {
            env_var: Some("SHOESTRING_MENU_FONT".into()),
        };
        assert!(err
            .to_string()
            .ends_with("set $SHOESTRING_MENU_FONT to a .ttf/.otf"));
    }
}
