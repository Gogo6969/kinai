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

    // Windows: give TEST binaries a comctl32 v6 manifest. The lib tests
    // link tauri's dialog/window code (they construct AppState since the
    // 0.2.81 failover tests), whose import table includes
    // comctl32::TaskDialogIndirect — an export that only exists in the
    // v6 side-by-side assembly, which the loader activates ONLY for
    // binaries carrying a manifest. The real app gets one from
    // tauri_build; bare cargo-test executables got none, so the loader
    // bound the ancient v5 comctl32 and every test run died at load with
    // STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139) before main. These
    // linker args apply to TEST targets only — the shipped app's
    // manifest handling is untouched.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0" processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#;
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
        let path = std::path::Path::new(&out_dir).join("kinai-test.manifest");
        std::fs::write(&path, manifest).expect("write test manifest");
        println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}", path.display());
    }

    tauri_build::build()
}
