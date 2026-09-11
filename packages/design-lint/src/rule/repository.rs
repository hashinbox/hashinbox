use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use proc_macro2::Span;

use super::{finding, production};
use crate::{
    Finding, Location, Policy, Result,
    source::{Workspace, slash},
};

pub(super) fn dependencies(workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>> {
    let packages = workspace
        .packages
        .iter()
        .map(|package| (package.name.as_str(), package))
        .collect::<BTreeMap<_, _>>();
    let ignored = &policy.dependency.ignored_packages;
    let mut output = Vec::new();
    for package in &workspace.packages {
        if ignored.contains(&package.name) {
            continue;
        }
        let from = policy.dependency.layer(&package.name, &package.root);
        for dependency in &package.dependencies {
            let Some(target) = packages.get(dependency.as_str()) else {
                continue;
            };
            if ignored.contains(&target.name) {
                continue;
            }
            if concrete_chain(package, workspace, policy)
                && concrete_chain(target, workspace, policy)
            {
                output.push(finding(
                    "dependency-direction",
                    format!("{} -> {}", package.name, target.name),
                    manifest_location(package),
                    "concrete chain crate may not depend on a sibling concrete chain crate",
                    "move shared values or capabilities to base, indexing, or wallets; \
                    compose concrete chains only in apps",
                ));
                continue;
            }
            let Some(from) = from else {
                continue;
            };
            let Some(to) = policy.dependency.layer(&target.name, &target.root) else {
                continue;
            };
            if from.may_depend_on.contains(&to.name) {
                continue;
            }
            output.push(finding(
                "dependency-direction",
                format!("{} -> {}", package.name, target.name),
                manifest_location(package),
                format!(
                    "layer `{}` may not depend on layer `{}`",
                    from.name, to.name
                ),
                "move the implementation to its owning layer or invert the dependency \
                    through an approved reusable trait",
            ));
        }
    }
    if let Some(cycle) = cycle(&workspace.packages, ignored) {
        output.push(finding(
            "dependency-direction",
            cycle.join(" -> "),
            manifest_location(packages[cycle[0].as_str()]),
            "local Cargo dependency cycle",
            "remove the reverse edge and keep dependencies flowing toward reusable layers",
        ));
    }
    Ok(output)
}

fn concrete_chain(
    package: &crate::source::Package,
    workspace: &Workspace,
    policy: &Policy,
) -> bool {
    let name = package.root.file_name().and_then(|name| name.to_str());
    name.is_some_and(|name| {
        !policy
            .repository
            .chain_exclusions
            .iter()
            .any(|excluded| excluded == name)
    }) && workspace.policy_roots.iter().any(|root| {
        package.root.parent() == Some(root.join(&policy.repository.chain_root).as_path())
    })
}

fn cycle(packages: &[crate::source::Package], ignored: &[String]) -> Option<Vec<String>> {
    fn visit(
        name: &str,
        map: &BTreeMap<&str, &crate::source::Package>,
        ignored: &[String],
        active: &mut Vec<String>,
        done: &mut BTreeSet<String>,
    ) -> Option<Vec<String>> {
        if let Some(start) = active.iter().position(|item| item == name) {
            let mut found = active[start..].to_vec();
            found.push(name.to_owned());
            return Some(found);
        }
        if done.contains(name) || ignored.iter().any(|item| item == name) {
            return None;
        }
        let package = map.get(name)?;
        active.push(name.to_owned());
        for dependency in &package.build_dependencies {
            if map.contains_key(dependency.as_str())
                && let Some(found) = visit(dependency, map, ignored, active, done)
            {
                return Some(found);
            }
        }
        active.pop();
        done.insert(name.to_owned());
        None
    }
    let map = packages
        .iter()
        .map(|package| (package.name.as_str(), package))
        .collect::<BTreeMap<_, _>>();
    let mut done = BTreeSet::new();
    for name in map.keys() {
        if let Some(found) = visit(name, &map, ignored, &mut Vec::new(), &mut done) {
            return Some(found);
        }
    }
    None
}

fn manifest_location(package: &crate::source::Package) -> Location {
    Location {
        path: package.root.join("Cargo.toml"),
        line: 1,
        column: 1,
        source: format!("[package] name = {:?}", package.name),
    }
}

pub(super) fn vocabulary(workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>> {
    let mut output = Vec::new();
    for source in workspace.production() {
        let path = slash(&source.path).to_ascii_lowercase();
        for owner in &policy.vocabulary.owners {
            if owner
                .allowed_paths
                .iter()
                .any(|allowed| path.contains(&allowed.to_ascii_lowercase()))
            {
                continue;
            }
            let text = production::text(source);
            let words = production_lines(&text)
                .enumerate()
                .flat_map(|(line, text)| {
                    words(text).into_iter().map(move |word| (line, text, word))
                });
            for (line, text, word) in words {
                if !owner
                    .words
                    .iter()
                    .any(|forbidden| forbidden.eq_ignore_ascii_case(&word))
                    || source.suppressed("owned-vocabulary", line + 1)
                {
                    continue;
                }
                let location = Location {
                    path: source.path.clone(),
                    line: line + 1,
                    column: 1,
                    source: text.trim().to_owned(),
                };
                output.push(finding(
                    "owned-vocabulary",
                    word.clone(),
                    location,
                    format!("chain-owned word `{word}` appears outside its owning chain"),
                    "use a chain-neutral term or move the code to the concrete \
                    chain/application boundary",
                ));
            }
        }
    }
    Ok(output)
}

fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .flat_map(super::rust::name_words)
        .collect()
}

fn production_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
}

pub(super) fn file_length(workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>> {
    Ok(workspace
        .production()
        .filter_map(|source| {
            let count = production::line_count(source);
            if count <= policy.repository.maximum_rust_lines {
                return None;
            }
            Some(finding(
                "file-length",
                source
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("source"),
                source.location(Span::call_site()),
                format!(
                    "Rust source contains {count} production lines; maximum is {}",
                    policy.repository.maximum_rust_lines
                ),
                "split the file by cohesive domain ownership",
            ))
        })
        .collect())
}

pub(super) fn forbidden_paths(workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>> {
    let mut output = Vec::new();
    for root in &workspace.roots {
        for forbidden in &policy.repository.forbidden_paths {
            let path = root.join(forbidden);
            if path.exists() {
                output.push(finding(
                    "forbidden-path",
                    forbidden,
                    path_location(&path),
                    format!("deleted architecture path `{forbidden}` still exists"),
                    "remove superseded code instead of retaining compatibility structure",
                ));
            }
        }
    }
    Ok(output)
}

pub(super) fn empty_directories(workspace: &Workspace, _policy: &Policy) -> Result<Vec<Finding>> {
    Ok(workspace
        .directories
        .iter()
        .filter(|directory| {
            std::fs::read_dir(directory)
                .ok()
                .is_some_and(|mut entries| entries.next().is_none())
        })
        .map(|directory| {
            finding(
                "empty-directory",
                directory.display().to_string(),
                path_location(directory),
                "repository-owned directory is empty",
                "remove the directory or add its required implementation",
            )
        })
        .collect())
}

pub(super) fn single_file_directories(
    workspace: &Workspace,
    _policy: &Policy,
) -> Result<Vec<Finding>> {
    let mut output = Vec::new();
    for directory in &workspace.directories {
        if !directory
            .components()
            .any(|component| component.as_os_str() == "src")
            || directory.file_name().is_some_and(|name| name == "src")
        {
            continue;
        }
        let entries = std::fs::read_dir(directory)
            .map_err(|error| crate::LintError::io("read", directory, error))?;
        let files = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.is_file()
                    && path.extension().and_then(|extension| extension.to_str()) == Some("rs")
            })
            .collect::<Vec<_>>();
        if files.len() == 1 {
            let only = &files[0];
            let name = only
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("source");
            output.push(finding(
                "single-file-directory",
                format!("{}/{}", directory.display(), name),
                path_location(only),
                format!(
                    "source directory `{}` contains only `{}`",
                    directory.display(),
                    name
                ),
                "keep the module beside its parent or add a second cohesive \
                    implementation file that justifies the directory",
            ));
        }
    }
    Ok(output)
}

pub(super) fn chain_layout(workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>> {
    let mut output = Vec::new();
    for root in &workspace.roots {
        let chains = root.join(&policy.repository.chain_root);
        let Ok(entries) = std::fs::read_dir(&chains) else {
            continue;
        };
        for entry in entries {
            let entry = entry.map_err(|error| crate::LintError::io("read", &chains, error))?;
            let chain = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !chain.is_dir() || policy.repository.chain_exclusions.contains(&name) {
                continue;
            }
            for required in &policy.repository.chain_directories {
                let expected = chain.join(required);
                if expected.is_dir() {
                    continue;
                }
                let file_form = chain.join(format!("{required}.rs"));
                let uses_file = file_form.is_file();
                let actual = relative(&chain, &file_form);
                let message = format!(
                    "chain `{name}` uses `{actual}` but the shared topology requires \
                    directory `{required}` with `{required}/mod.rs`"
                );
                let message = if uses_file {
                    message
                } else {
                    format!("chain `{name}` is missing required directory `{required}`")
                };
                let location = if uses_file { file_form } else { expected };
                output.push(finding(
                    "chain-layout",
                    format!("{name}/{required}"),
                    path_location(&location),
                    message,
                    "use the common chain directory topology; keep protocol differences \
                    in files within those directories",
                ));
            }
            for required in &policy.repository.chain_skeleton {
                let expected = chain.join(required);
                if expected.is_file() {
                    continue;
                }
                if expected.parent().is_some_and(|parent| !parent.is_dir()) {
                    continue;
                }
                let file_form = required
                    .strip_suffix("/mod.rs")
                    .map(|module| chain.join(format!("{module}.rs")))
                    .filter(|path| path.is_file());
                let actual = relative(&chain, file_form.as_ref().unwrap_or(&expected));
                let message = format!(
                    "chain `{name}` uses `{actual}` but the shared chain layout \
                    requires `{required}`"
                );
                let message = if file_form.is_some() {
                    message
                } else {
                    format!("chain `{name}` is missing required path `{required}`")
                };
                let location = file_form.unwrap_or(expected);
                output.push(finding(
                    "chain-layout",
                    format!("{name}/{required}"),
                    path_location(&location),
                    message,
                    "use the common chain skeleton; keep protocol-specific additions below \
                    those ownership boundaries",
                ));
            }
            let expected = policy
                .repository
                .chain_directories
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            for directory in nested_directories(&chain.join("src"))? {
                let relative = relative(&chain, &directory);
                if expected.contains(relative.as_str()) {
                    continue;
                }
                output.push(finding(
                    "chain-layout",
                    format!("{name}/{relative}"),
                    path_location(&directory),
                    format!("chain `{name}` has unexpected nested directory `{relative}`"),
                    "move protocol-owned files into the common chain directories; do \
                    not add another architectural layer",
                ));
            }
        }
    }
    Ok(output)
}

fn nested_directories(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut output = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(output);
    };
    for entry in entries {
        let entry = entry.map_err(|error| crate::LintError::io("read", root, error))?;
        let path = entry.path();
        if path.is_dir() {
            output.push(path.clone());
            output.extend(nested_directories(&path)?);
        }
    }
    Ok(output)
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(crate::source::slash)
        .unwrap_or_else(|_| "module file".to_owned())
}

fn path_location(path: &Path) -> Location {
    Location {
        path: path.to_owned(),
        line: 1,
        column: 1,
        source: String::new(),
    }
}
