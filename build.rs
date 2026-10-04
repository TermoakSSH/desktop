//! Resources of the Windows .exe: the icon (resource 1, the one GPUI loads)
//! and, with mingw, our manifest (per-monitor DPI, UTF-8, long paths).

fn main() {
    println!("cargo:rerun-if-changed=resources/windows");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let rc = if msvc {
        // GPUI already embeds a manifest resource (per-monitor DPI, common
        // controls); a second one from the MSVC linker makes the link fail
        // with a duplicate resource, so the linker must not generate one.
        println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
        "resources/windows/app.rc"
    } else {
        "resources/windows/app-gnu.rc"
    };
    embed_resource::compile(rc, embed_resource::NONE)
        .manifest_optional()
        .expect("could not compile the Windows resources (windres or rc.exe is needed)");
}
