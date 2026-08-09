//! Runtime font loading.
//!
//! Large CJK-capable fonts (e.g. Maple Mono, ~20 MB per weight) are no longer
//! embedded at build time. Instead the user drops font files into the fonts
//! folder; at startup we register every file with Slint's shared fontique
//! collection, and the family names become selectable in
//! Settings → Interface → Terminal font.
//!
//! Supported formats: `.ttf` / `.otf` (single face) and `.ttc` / `.otc`
//! (collections — every face inside is registered; fontique and fontdb both
//! enumerate collection indices natively).
//!
//! Location differs by platform:
//! - **Windows** (installed or portable): `<exe_dir>/config/fonts` — the app
//!   is fully self-contained, nothing is ever written to AppData.
//! - **macOS / Linux**: keep the original per-user OS config dir
//!   (`~/.config/rudder/rudder/fonts` etc.).
//!
//! The embedded set stays minimal: JetBrains Mono (Regular/Bold/Italic) +
//! Meatshell Mono (Regular/Bold) + Material Icons.

use std::path::{Path, PathBuf};

/// Windows: `<exe_dir>/config/fonts`, always beside the executable.
#[cfg(target_os = "windows")]
pub(crate) fn external_fonts_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("config").join("fonts")))
        .unwrap_or_else(|| PathBuf::from("config/fonts"))
}

/// macOS / Linux: the per-user OS config dir, as before.
#[cfg(not(target_os = "windows"))]
pub(crate) fn external_fonts_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "rudder", "rudder")
        .map(|d| d.config_dir().join("fonts"))
        .unwrap_or_else(|| PathBuf::from("fonts"))
}

/// Font extensions accepted from the fonts dir: `.ttf` / `.otf` and the
/// collection formats `.ttc` / `.otc` (a single file holding several faces).
/// Case-insensitive; sorted for deterministic order.
pub(crate) fn scan_font_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| {
                    ["ttf", "otf", "ttc", "otc"]
                        .iter()
                        .any(|ext| x.eq_ignore_ascii_case(ext))
                })
        })
        .collect();
    files.sort();
    files
}

/// Family names inside one font file (first English family of each face,
/// deduplicated and sorted). Used by tests only.
#[cfg(test)]
pub(crate) fn family_names_in(font_path: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(font_path) else {
        return Vec::new();
    };
    let mut db = fontdb::Database::new();
    db.load_font_data(bytes);
    let mut names: Vec<String> = db
        .faces()
        .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Monospace families installed on the system, for the Settings font picker.
/// Sorted, deduplicated. Slint's fontique collection already contains the
/// system fonts, so choosing one only sets the family name — no registration
/// needed.
pub(crate) fn system_monospace_families() -> Vec<String> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let mut names: Vec<String> = db
        .faces()
        .filter(|f| f.monospaced)
        .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Ensure the fonts dir exists and register every font file in it with
/// Slint's shared collection. Returns the registered family names
/// (deduplicated, sorted) for the Settings font picker.
///
/// Must run after the Slint platform is initialized (`AppWindow::new`), since
/// `shared_collection()` requires the global font context.
pub(crate) fn load_external_fonts(fonts_dir: &Path) -> Vec<String> {
    let _ = std::fs::create_dir_all(fonts_dir);
    let mut families: Vec<String> = Vec::new();
    for path in scan_font_files(fonts_dir) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let blob = slint::fontique_010::fontique::Blob::new(std::sync::Arc::new(bytes));
        let mut collection = slint::fontique_010::shared_collection();
        let registered = collection.register_fonts(blob, None);
        for (family_id, _) in registered {
            if let Some(name) = collection.family_name(family_id) {
                families.push(name.to_string());
            }
        }
    }
    families.sort();
    families.dedup();
    families
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir_with_font() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("ui/fonts/JetBrainsMono-Regular.ttf");
        let dst = dir.path().join("JetBrainsMono-Regular.ttf");
        std::fs::copy(&src, &dst).unwrap();
        (dir, dst)
    }

    #[test]
    fn scan_filters_to_font_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.txt"), "not a font").unwrap();
        std::fs::write(dir.path().join("note.TTF"), "fake").unwrap();
        std::fs::write(dir.path().join("font.otf"), "fake").unwrap();
        std::fs::write(dir.path().join("collection.ttc"), "fake").unwrap();
        std::fs::write(dir.path().join("collection.OTC"), "fake").unwrap();
        std::fs::write(dir.path().join("font.woff2"), "fake").unwrap();
        let files = scan_font_files(dir.path());
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["collection.OTC", "collection.ttc", "font.otf", "note.TTF"]
        );
    }

    #[test]
    fn scan_missing_dir_returns_empty() {
        assert!(scan_font_files(&PathBuf::from("no/such/dir")).is_empty());
    }

    #[test]
    fn family_names_parses_jetbrains_mono() {
        let (_tmp, font) = temp_dir_with_font();
        let names = family_names_in(&font);
        assert!(
            names.iter().any(|n| n == "JetBrains Mono"),
            "expected JetBrains Mono, got {names:?}"
        );
    }

    #[test]
    fn external_dir_points_at_fonts() {
        let dir = external_fonts_dir();
        assert!(dir.ends_with("fonts"), "unexpected dir {dir:?}");
        #[cfg(target_os = "windows")]
        assert!(
            dir.to_string_lossy().contains("config"),
            "Windows fonts must live under <exe_dir>/config/fonts, got {dir:?}"
        );
    }
}
