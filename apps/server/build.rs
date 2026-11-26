//! Embeds every file the server serves: `ui/dist/` at the root and `assets/`
//! at `/assets/…`. Writes their `(url path, include_bytes!(file))` table
//! (`ui::FILES`), so a new file in either folder needs no code.
//!
//! Also sets `HASHINBOX_VERSION`: the build environment's, else
//! `git describe --tags --always --dirty`, else the crate version.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
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

/// The version: `HASHINBOX_VERSION` if set, else the tag description of the
/// git checkout at or above `root`, else the crate version.
fn version(root: &Path) -> String {
    println!("cargo:rerun-if-env-changed=HASHINBOX_VERSION");
    if let Ok(version) = env::var("HASHINBOX_VERSION") {
        return version;
    }
    let crate_version = env::var("CARGO_PKG_VERSION").unwrap();
    let Some(git) = root
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|git| git.exists())
    else {
        return crate_version;
    };
    for watched in ["HEAD", "refs"].map(|name| git.join(name)) {
        if watched.exists() {
            println!("cargo:rerun-if-changed={}", watched.display());
        }
    }
    let Ok(output) = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .current_dir(git.parent().unwrap())
        .output()
    else {
        return crate_version;
    };
    let described = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || described.is_empty() {
        return crate_version;
    }
    described
}

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-env=HASHINBOX_VERSION={}", version(&root));
    let mut rows = Vec::new();
    for (folder, prefix) in [("ui/dist", ""), ("assets", "/assets")] {
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
