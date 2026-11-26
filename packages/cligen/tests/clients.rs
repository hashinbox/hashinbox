//! `apps/server/clients/<language>/` with `docs/openapi.json`'s generated
//! files in it type-checks: `tsc` (the dashboard's copy), `cargo check
//! --offline`, `go vet`, `python3 -m py_compile` and `mypy` (else
//! `pyright`), `php -l` and `phpstan --level=max`. Each runs on a scratch
//! copy, so the folders are not touched; a tool that is not installed is
//! skipped with a note. The options are `apps/server/build.rs`'s.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cligen::Spec;

/// The repository root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `from` copied into `to`, build output and locks left out.
fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("a scratch directory");
    for entry in fs::read_dir(from).expect("a client folder") {
        let path = entry.expect("an entry").path();
        let name = path.file_name().expect("a name").to_owned();
        if [
            "target",
            "node_modules",
            "Cargo.lock",
            "vendor",
            "composer.lock",
        ]
        .iter()
        .any(|skip| name == *skip)
        {
            continue;
        }
        if path.is_dir() {
            copy(&path, &to.join(name));
        } else {
            fs::copy(&path, to.join(name)).expect("a copy");
        }
    }
}

/// Whether `tool` is on the path.
fn installed(tool: &str) -> bool {
    Command::new("which")
        .arg(tool)
        .output()
        .is_ok_and(|out| out.status.success())
}

#[test]
fn clients_type_check() {
    let text = fs::read_to_string(root().join("docs/openapi.json")).expect("docs/openapi.json");
    let spec = Spec::from_openapi(&text).expect("it resolves");
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cligen-clients");
    let _ = fs::remove_dir_all(&scratch);
    let mut failures = Vec::new();
    let go = cligen::go::Go {
        package: "hashinbox".into(),
    };
    let php = cligen::php::Php {
        namespace: "Hashinbox".into(),
    };
    let languages = [
        (
            "typescript",
            "src",
            cligen::typescript::Typescript.emit(&spec),
        ),
        ("go", "", go.emit(&spec)),
        ("rust", "src", cligen::rust::Rust.emit(&spec)),
        ("python", "hashinbox", cligen::python::Python.emit(&spec)),
        ("php", "src", php.emit(&spec)),
    ];
    for (language, generated, files) in languages {
        let dir = scratch.join(language);
        copy(&root().join("apps/server/clients").join(language), &dir);
        for (path, text) in files {
            fs::write(dir.join(generated).join(path), text).expect("a generated file");
        }
        if language == "python" {
            failures.extend(python(&dir));
            continue;
        }
        if language == "php" {
            failures.extend(php_checks(&dir));
            continue;
        }
        let command = match language {
            "typescript" => {
                let tsc = root().join("apps/server/ui/node_modules/.bin/tsc");
                (tsc.exists() && installed("node")).then(|| {
                    let mut command = Command::new(tsc);
                    command.args(["-p", "."]).current_dir(&dir);
                    command
                })
            }
            "rust" => installed("cargo").then(|| {
                fs::copy(root().join("Cargo.lock"), dir.join("Cargo.lock")).expect("a lock");
                let mut command = Command::new("cargo");
                command
                    .args(["check", "--offline", "--quiet"])
                    .env("CARGO_TARGET_DIR", root().join("target/clients-check"))
                    .current_dir(&dir);
                command
            }),
            _ => installed("go").then(|| {
                let mut command = Command::new("go");
                command.args(["vet", "./..."]).current_dir(&dir);
                command
            }),
        };
        let Some(mut command) = command else {
            println!("{language}: compiler not installed, skipped");
            continue;
        };
        let out = command.output().expect("the compiler runs");
        let report = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        if out.status.success() && !report.contains("warning") {
            println!("{language}: type-checks");
        } else {
            failures.push(format!("{language}:\n{report}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The Python client at `dir` checked: each file compiles, and the package
/// type-checks (`mypy`, else `pyright`, strict by its `pyproject.toml`).
fn python(dir: &Path) -> Vec<String> {
    let files: Vec<PathBuf> = fs::read_dir(dir.join("hashinbox"))
        .expect("the package")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "py"))
        .collect();
    let mut compile = Command::new("python3");
    compile
        .args(["-m", "py_compile"])
        .args(files)
        .env("PYTHONPYCACHEPREFIX", dir.join(".pycache"));
    let mut commands = vec![("py_compile", installed("python3").then_some(compile))];
    let check = if installed("mypy") {
        let mut command = Command::new("mypy");
        command.args(["--strict", "hashinbox"]);
        Some(command)
    } else if installed("pyright") {
        let mut command = Command::new("pyright");
        command.arg("hashinbox");
        Some(command)
    } else {
        None
    };
    commands.push(("type check", check));
    let mut failures = Vec::new();
    for (name, command) in commands {
        let Some(mut command) = command else {
            println!("python {name}: not installed, skipped");
            continue;
        };
        let out = command.current_dir(dir).output().expect("the checker runs");
        if out.status.success() {
            println!("python {name}: passes");
        } else {
            let report = String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr);
            failures.push(format!("python {name}:\n{report}"));
        }
    }
    failures
}

/// The PHP client at `dir` checked: each file of `src/` lints (`php -l`),
/// and `src/` passes `phpstan analyse --level=max`.
fn php_checks(dir: &Path) -> Vec<String> {
    if !installed("php") {
        println!("php: not installed, skipped");
        return Vec::new();
    }
    let mut failures = Vec::new();
    let mut files: Vec<PathBuf> = fs::read_dir(dir.join("src"))
        .expect("src/")
        .map(|entry| entry.expect("an entry").path())
        .collect();
    files.sort();
    for file in files {
        let out = Command::new("php")
            .arg("-l")
            .arg(&file)
            .output()
            .expect("php runs");
        if !out.status.success() {
            failures.push(format!(
                "php -l {}:\n{}",
                file.display(),
                String::from_utf8_lossy(&out.stdout)
            ));
        }
    }
    println!(
        "php -l: {}",
        if failures.is_empty() {
            "passes"
        } else {
            "fails"
        }
    );
    if !installed("phpstan") {
        println!("php phpstan: not installed, skipped");
        return failures;
    }
    let out = Command::new("phpstan")
        .args([
            "analyse",
            "--level=max",
            "--no-progress",
            "--memory-limit=2G",
            "src",
        ])
        .current_dir(dir)
        .output()
        .expect("phpstan runs");
    if out.status.success() {
        println!("php phpstan: passes");
    } else {
        let report = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        failures.push(format!("php phpstan:\n{report}"));
    }
    failures
}
