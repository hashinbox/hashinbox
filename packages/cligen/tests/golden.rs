//! `tests/fixtures/mini.json` generated in every language and compared with
//! the committed files under `tests/golden/<language>/`.
//!
//! To accept new output: `CLIGEN_BLESS=1 cargo test -p cligen --test golden`
//! rewrites them (and deletes files no longer generated); review the diff.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The committed files of `dir`: name → contents.
fn committed(dir: &Path) -> BTreeMap<PathBuf, String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .map(|entry| entry.expect("a directory entry").path())
        .map(|path| {
            (
                PathBuf::from(path.file_name().expect("a file name")),
                fs::read_to_string(&path).expect("text"),
            )
        })
        .collect()
}

#[test]
fn fixture_matches_golden_files() {
    let spec = common::spec("tests/fixtures/mini.json");
    let bless = std::env::var_os("CLIGEN_BLESS").is_some();
    let mut stale = Vec::new();
    for (language, emit) in common::languages() {
        let files = emit(&spec);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(language);
        if bless {
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("the golden directory");
            for (path, text) in &files {
                fs::write(dir.join(path), text).expect("a golden file");
            }
            continue;
        }
        let have = committed(&dir);
        for (path, text) in &files {
            if have.get(path) != Some(text) {
                stale.push(format!("{language}/{}", path.display()));
            }
        }
        stale.extend(
            have.keys()
                .filter(|path| !files.contains_key(*path))
                .map(|path| format!("{language}/{} (no longer generated)", path.display())),
        );
    }
    assert!(
        stale.is_empty(),
        "golden files differ (CLIGEN_BLESS=1 rewrites them): {stale:#?}"
    );
}
