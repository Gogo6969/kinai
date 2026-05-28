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

    // Re-run build.rs when HEAD moves so the embedded git short-hash
    // doesn't go stale after a commit that didn't touch src/. Before
    // this, `rerun-if-changed=src` was the ONLY trigger — committing
    // (which doesn't change file mtimes under src/) left the cached
    // hash in place, so a rebuild after `git commit` would embed the
    // PREVIOUS commit's hash. That's how a v0.2.47 binary ended up
    // reporting commit 458a300 (the v0.2.46 commit) even though it
    // had the v0.2.47 source compiled in. We watch .git/HEAD AND the
    // ref it points to (the file that actually changes on commit).
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD") {
        if let Some(ref_rel) = head.strip_prefix("ref: ").map(str::trim) {
            println!("cargo:rerun-if-changed=.git/{ref_rel}");
        }
    }

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
