use std::{env, fs, path::Path, process::Command};

fn main() {
    let sidecar_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/maven/sidecar");

    // Rebuild only when the sidecar sources change. The Gradle build outputs
    // (`build/`, `.gradle/`) are intentionally not tracked, otherwise every
    // `cargo build` would re-run the build script.
    for path in [
        "src/main/java",
        "src/main/resources",
        "build.gradle",
        "settings.gradle",
        "gradle/wrapper",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            sidecar_dir.join(path).display()
        );
    }

    let jar_name = "caffeine-ls-maven-sidecar.jar";

    let mut command = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.arg("/c").arg(sidecar_dir.join("gradlew.bat"));
        cmd
    } else {
        Command::new(sidecar_dir.join("gradlew"))
    };

    let output = command
        .current_dir(&sidecar_dir)
        .args(["-p", ".", "build"])
        .output()
        .expect("failed to spawn the Gradle wrapper");

    if !output.status.success() {
        println!(
            "cargo:error=Failed to build the Maven sidecar plugin (Gradle exited with {}). \
             Make sure a JDK is available and the Gradle distribution can be downloaded \
             on the first build.",
            output.status.code().unwrap_or(-1)
        );
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        std::process::exit(1);
    }

    let built_jar = sidecar_dir.join("build/libs").join(jar_name);
    if !built_jar.exists() {
        println!(
            "cargo:error=Maven sidecar jar missing after the Gradle build: {}",
            built_jar.display()
        );
        std::process::exit(1);
    }

    let out_dir_value = env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    let out_dir = Path::new(&out_dir_value);
    let dest = out_dir.join(jar_name);
    fs::copy(&built_jar, &dest).expect("failed to copy the sidecar jar into OUT_DIR");
}
