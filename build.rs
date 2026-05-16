fn main() {
    // Stamp the binary with the current build time so the Settings page can
    // show "you're on the build from <date>". Re-emitting the env var on
    // every build ensures it isn't cached when cargo would otherwise skip
    // recompilation of touched files.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=KINAI_BUILD_TIME={now}");
    println!("cargo:rerun-if-changed=src");

    // Short git commit hash, if a repo is available.
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=KINAI_GIT_COMMIT={commit}");

    tauri_build::build()
}
