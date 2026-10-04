//! Resources of the Windows .exe: the icon (resource 1, the one GPUI loads)
//! and the manifest, which declares per-monitor DPI awareness.

fn main() {
    println!("cargo:rerun-if-changed=resources/windows");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let rc = if msvc {
        // The MSVC linker generates its own manifest: give it ours.
        let manifest = std::path::Path::new("resources/windows/app.manifest")
            .canonicalize()
            .expect("resources/windows/app.manifest is missing");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
        "resources/windows/app.rc"
    } else {
        "resources/windows/app-gnu.rc"
    };
    embed_resource::compile(rc, embed_resource::NONE)
        .manifest_optional()
        .expect("could not compile the Windows resources (windres or rc.exe is needed)");
}
