//! Embeds `app.manifest` (a copy of `src\BetterRecall.Agent\app.manifest`) into the agent exe, as the C# build does.
//! DXGI Desktop Duplication (`DuplicateOutput1`) needs the Windows 10 `supportedOS` entry and PerMonitorV2 DPI
//! awareness; without them every output stays unavailable.

fn main() {
    let manifest =
        std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("app.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
