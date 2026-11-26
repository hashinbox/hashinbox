//! The generated files are formatter-clean: `prettier --check` (TypeScript,
//! the dashboard's copy), `gofmt -l` (Go), `rustfmt --check` (Rust) and
//! `ruff format --check` or `black --check` (Python) and
//! `php-cs-fixer fix --dry-run --rules=@PER-CS2.0` (PHP), on the golden
//! files and on `docs/openapi.json`'s output. A formatter that is not
//! installed (PHP's: or `php` itself) is skipped with a note, never a
//! failure.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The command checking a directory of `language`'s files, if its tool is
/// installed.
fn checker(language: &str, dir: &Path) -> Option<Command> {
    let installed = |tool: &str| {
        Command::new("which")
            .arg(tool)
            .output()
            .is_ok_and(|out| out.status.success())
    };
    let files = || {
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .expect("a directory")
            .map(|entry| entry.expect("an entry").path())
            .collect();
        files.sort();
        files
    };
    match language {
        "typescript" => {
            let prettier = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../apps/server/ui/node_modules/.bin/prettier");
            if !prettier.exists() || !installed("node") {
                return None;
            }
            let mut command = Command::new(prettier);
            command
                .args(["--check", "--no-config", "--no-editorconfig"])
                .arg(dir)
                .current_dir(dir);
            Some(command)
        }
        "go" if installed("gofmt") => {
            let mut command = Command::new("gofmt");
            command.arg("-l").arg(dir);
            Some(command)
        }
        "rust" if installed("rustfmt") => {
            let mut command = Command::new("rustfmt");
            command.args(["--check", "--edition", "2024"]).args(files());
            Some(command)
        }
        "python" if installed("ruff") => {
            let mut command = Command::new("ruff");
            command
                .args([
                    "format",
                    "--check",
                    "--isolated",
                    "--target-version",
                    "py311",
                ])
                .arg(dir);
            Some(command)
        }
        "python" if installed("black") => {
            let mut command = Command::new("black");
            command
                .args(["--check", "--quiet", "--target-version", "py311"])
                .arg(dir);
            Some(command)
        }
        "php" if installed("php") && installed("php-cs-fixer") => {
            let mut command = Command::new("php-cs-fixer");
            command
                .args([
                    "fix",
                    "--dry-run",
                    "--diff",
                    "--using-cache=no",
                    "--rules=@PER-CS2.0",
                ])
                .arg(dir);
            Some(command)
        }
        _ => None,
    }
}

#[test]
fn generated_files_are_formatter_clean() {
    let spec = common::spec("../../docs/openapi.json");
    let mut failures = Vec::new();
    for (language, emit) in common::languages() {
        let real = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("cligen-spec")
            .join(language);
        let _ = fs::remove_dir_all(&real);
        fs::create_dir_all(&real).expect("a scratch directory");
        for (path, text) in emit(&spec) {
            fs::write(real.join(path), text).expect("a scratch file");
        }
        let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(language);
        for dir in [golden, real] {
            let Some(mut command) = checker(language, &dir) else {
                println!("{language}: formatter not installed, skipped");
                break;
            };
            let out = command.output().expect("the formatter runs");
            if !out.status.success() || (language == "go" && !out.stdout.is_empty()) {
                let report = String::from_utf8_lossy(&out.stdout).into_owned()
                    + &String::from_utf8_lossy(&out.stderr);
                failures.push(format!("{language} {}:\n{report}", dir.display()));
            } else {
                println!("{language}: {} is formatter-clean", dir.display());
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
