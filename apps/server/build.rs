//! Embeds every file the server serves: `ui/dist/` at `/ui/…` and `assets/`
//! at `/assets/…`. Writes their `(url path, include_bytes!(file))` table
//! (`ui::FILES`), so a new file in either folder needs no code.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

/// Every file under `folder`, recursively.
fn files(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().path())
        .flat_map(|path| {
            if path.is_dir() {
                files(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut rows = Vec::new();
    for (folder, prefix) in [("ui/dist", "/ui"), ("assets", "/assets")] {
        let folder = root.join(folder);
        println!("cargo:rerun-if-changed={}", folder.display());
        for path in files(&folder) {
            let relative = path.strip_prefix(&folder).unwrap().to_str().unwrap();
            let url = format!("{prefix}/{}", relative.replace('\\', "/"));
            rows.push(format!(
                "({url:?}, include_bytes!({:?})),",
                path.display().to_string()
            ));
        }
    }
    rows.sort();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("files.rs");
    fs::write(out, format!("&[\n{}\n]", rows.join("\n"))).unwrap();
}
