use crate::{LintError, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub struct Package {
    pub name: String,
    pub root: PathBuf,
    /// Local normal, build and development dependency package names.
    pub dependencies: Vec<String>,
    /// Local normal and build edges; Cargo permits development cycles.
    pub build_dependencies: Vec<String>,
}

pub(super) fn manifests(files: &BTreeSet<PathBuf>) -> Result<BTreeMap<PathBuf, toml::Value>> {
    let mut paths = BTreeSet::new();
    for file in files {
        for directory in file.ancestors().skip(1) {
            let path = directory.join("Cargo.toml");
            if path.is_file() {
                paths.insert(path);
            }
        }
    }
    let mut output = BTreeMap::new();
    while !paths.is_empty() {
        for path in std::mem::take(&mut paths) {
            let root = path.parent().unwrap_or(Path::new(".")).to_owned();
            if output.contains_key(&root) {
                continue;
            }
            let text =
                fs::read_to_string(&path).map_err(|error| LintError::io("read", &path, error))?;
            let manifest: toml::Value = toml::from_str(&text).map_err(|error| {
                LintError::configuration(format!("failed to parse {}: {error}", path.display()))
            })?;
            output.insert(root, manifest);
        }
        for (root, manifest) in &output {
            let entries = dependencies(manifest).flat_map(|(_, entries)| entries);
            for (alias, entry) in entries {
                let Some(path) = local_path(alias, entry, root, &output)? else {
                    continue;
                };
                let discovered = path
                    .ancestors()
                    .filter(|ancestor| !output.contains_key(*ancestor))
                    .map(|ancestor| ancestor.join("Cargo.toml"))
                    .filter(|manifest| manifest.is_file());
                paths.extend(discovered);
            }
        }
    }
    Ok(output)
}

fn dependencies(manifest: &toml::Value) -> impl Iterator<Item = (&'static str, &toml::Table)> {
    let tables = std::iter::once(manifest).chain(
        manifest
            .get("target")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flat_map(|table| table.values()),
    );
    tables.flat_map(|table| {
        ["dependencies", "build-dependencies", "dev-dependencies"]
            .into_iter()
            .filter_map(move |kind| Some((kind, table.get(kind)?.as_table()?)))
    })
}

pub(super) fn inventory(manifests: &BTreeMap<PathBuf, toml::Value>) -> Result<Vec<Package>> {
    let names = manifests
        .iter()
        .filter_map(|(root, manifest)| {
            Some((
                root.clone(),
                manifest.get("package")?.get("name")?.as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut packages = Vec::new();
    for (root, name) in &names {
        let manifest = &manifests[root];
        let mut all = BTreeSet::new();
        let mut build = BTreeSet::new();
        for (kind, dependencies) in dependencies(manifest) {
            for (alias, entry) in dependencies {
                let Some(path) = local_path(alias, entry, root, manifests)? else {
                    continue;
                };
                let Some(target) = names.get(&path) else {
                    continue;
                };
                all.insert(target.clone());
                if kind != "dev-dependencies" {
                    build.insert(target.clone());
                }
            }
        }
        packages.push(Package {
            name: name.clone(),
            root: root.clone(),
            dependencies: all.into_iter().collect(),
            build_dependencies: build.into_iter().collect(),
        });
    }
    Ok(packages)
}

fn local_path(
    alias: &str,
    entry: &toml::Value,
    root: &Path,
    manifests: &BTreeMap<PathBuf, toml::Value>,
) -> Result<Option<PathBuf>> {
    let (base, entry) = if entry.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
        let inherited = root
            .ancestors()
            .find_map(|ancestor| Some((ancestor, manifests.get(ancestor)?.get("workspace")?)))
            .and_then(|(ancestor, workspace)| {
                Some((ancestor, workspace.get("dependencies")?.get(alias)?))
            });
        inherited.ok_or_else(|| {
            LintError::configuration(format!(
                "missing workspace dependency `{alias}` for {}",
                root.display()
            ))
        })?
    } else {
        (root, entry)
    };
    let Some(path) = entry.get("path").and_then(toml::Value::as_str) else {
        return Ok(None);
    };
    let path = base.join(path);
    let canonical = fs::canonicalize(&path)
        .map_err(|error| LintError::io("resolve dependency", &path, error))?;
    Ok(Some(canonical))
}
