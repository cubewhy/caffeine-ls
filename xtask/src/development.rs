use std::{env, path::PathBuf};

use xshell::{Shell, cmd};

pub fn run_vscode() -> anyhow::Result<()> {
    let sh = Shell::new()?;

    let root_dir = env::var("CARGO_WORKSPACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| sh.current_dir());

    println!("🚀 Step 1: Building Caffeine LS...");

    cmd!(sh, "cargo build -p caffeine-ls").run()?;

    println!("📦 Step 2: Copying binary to extension directory...");

    let exe_suffix = env::consts::EXE_SUFFIX;
    let binary_name = format!("caffeine-ls{exe_suffix}");

    let source_bin = root_dir.join("target").join("debug").join(&binary_name);
    let extension_dir = root_dir.join("editors").join("code");
    let bin_dir = extension_dir.join("bin");

    sh.create_dir(&bin_dir)?;

    let target_bin = bin_dir.join(&binary_name);
    sh.copy_file(source_bin, target_bin)?;

    println!("⚙️ Step 3: Compiling Extension...");
    sh.change_dir(&extension_dir);

    if !extension_dir.join("node_modules").exists() {
        println!("  - Installing dependencies...");
        cmd!(sh, "pnpm install").run()?;
    }

    cmd!(sh, "pnpm run compile").run()?;

    println!("💻 Step 4: Launching VS Code...");

    sh.set_var("RUST_BACKTRACE", "1");
    sh.set_var("CAFFEINE_LS_LOG", "debug");

    cmd!(sh, "code --extensionDevelopmentPath={extension_dir}").run()?;

    Ok(())
}
