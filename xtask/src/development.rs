use std::{env, path::PathBuf};

use anyhow::Context;
use which::which;
use xshell::{Shell, cmd};

pub fn run_vscode(code_exec: Option<&str>, cargo_options: Vec<String>) -> anyhow::Result<()> {
    let sh = Shell::new()?;

    let root_dir = env::var("CARGO_WORKSPACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| sh.current_dir());

    let profile = if cargo_options
        .iter()
        .any(|opt| opt == "--release" || opt == "-r")
    {
        "release"
    } else {
        "debug"
    };

    cmd!(sh, "cargo build -p caffeine-ls {cargo_options...}").run()?;

    let exe_suffix = env::consts::EXE_SUFFIX;
    let binary_name = format!("caffeine-ls{exe_suffix}");

    let target_base_dir = env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root_dir.join("target"));

    let source_bin = target_base_dir.join(profile).join(&binary_name);
    let extension_dir = root_dir.join("editors").join("code");
    let bin_dir = extension_dir.join("bin");

    sh.create_dir(&bin_dir)?;

    let target_bin = bin_dir.join(&binary_name);
    sh.copy_file(source_bin, target_bin)?;

    sh.change_dir(&extension_dir);

    if !extension_dir.join("node_modules").exists() {
        println!("  - Installing dependencies...");
        cmd!(sh, "pnpm install").env("CI", "true").run()?;
    }

    cmd!(sh, "pnpm run compile").env("CI", "true").run()?;

    sh.set_var("RUST_BACKTRACE", "1");
    sh.set_var("CAFFEINE_LS_LOG", "debug");

    let code_exec = code_exec
        .and_then(|name| which(name).ok())
        .or_else(find_code_installation)
        .context("No VS Code executable found")?;

    let extension_dir = extension_dir.to_string_lossy();
    let extension_args = vec!["--extensionDevelopmentPath", &extension_dir];

    cmd!(sh, "{code_exec}").args(&extension_args).run()?;

    Ok(())
}

fn find_code_installation() -> Option<PathBuf> {
    let executables = ["code", "codium"];

    for exe in executables {
        if let Ok(path) = which(exe) {
            return Some(path);
        }
    }

    None
}
