//! Embeds the agent's Win32 resources into the exe, as the C# build did: the application icon
//! (`assets\icons\app.ico`), version information, and `app.manifest` (taken over from the C# agent).
//! DXGI Desktop Duplication (`DuplicateOutput1`) needs the manifest's Windows 10 `supportedOS` entry and
//! PerMonitorV2 DPI awareness; without them every output stays unavailable.
//!
//! The resource script is compiled with the Windows SDK's `rc.exe` (the `RC` environment variable, or the
//! newest `Windows Kits\10\bin\<version>\x64\rc.exe`) and the `.res` is passed to the linker. No crate needed.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let manifest = crate_dir.join("app.manifest");
    let icon = crate_dir.join("..\\..\\..\\assets\\icons\\app.ico");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-env-changed=RC");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let rc_path = out.join("agent.rc");
    std::fs::write(&rc_path, resource_script(&version, &icon, &manifest)).unwrap();
    let res_path = out.join("agent.res");
    let rc = find_rc().unwrap_or_else(|| {
        panic!("rc.exe not found: install the Windows 10/11 SDK or set RC to its path")
    });
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status()
        .unwrap_or_else(|e| panic!("running {}: {e}", rc.display()));
    assert!(status.success(), "rc.exe failed on {}", rc_path.display());

    // The manifest comes from the .res; no linker-generated one beside it.
    println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
    println!("cargo:rustc-link-arg-bins={}", res_path.display());
}

fn rc_string(p: &Path) -> String {
    p.display().to_string().replace('\\', "\\\\")
}

fn resource_script(version: &str, icon: &Path, manifest: &Path) -> String {
    let core = version.split(['-', '+']).next().unwrap_or("0.0.0");
    let mut nums: Vec<u16> = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
    nums.resize(4, 0);
    let commas = format!("{},{},{},{}", nums[0], nums[1], nums[2], nums[3]);
    let dotted = format!("{}.{}.{}.{}", nums[0], nums[1], nums[2], nums[3]);
    format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "000004b0"
    BEGIN
      VALUE "CompanyName", "BetterRecall contributors"
      VALUE "FileDescription", "BetterRecall.Agent"
      VALUE "FileVersion", "{dotted}"
      VALUE "InternalName", "BetterRecall.Agent"
      VALUE "LegalCopyright", "BetterRecall contributors, AGPL-3.0-only"
      VALUE "OriginalFilename", "BetterRecall.Agent.exe"
      VALUE "ProductName", "BetterRecall"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0, 1200
  END
END
"#,
        icon = rc_string(icon),
        manifest = rc_string(manifest),
    )
}

fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RC")
        && Path::new(&p).is_file()
    {
        return Some(PathBuf::from(p));
    }
    let kits = std::env::var("ProgramFiles(x86)")
        .unwrap_or_else(|_| r"C:\Program Files (x86)".to_string());
    let bin = Path::new(&kits).join(r"Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&bin)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join(r"x64\rc.exe").is_file())
        .collect();
    versions.sort();
    versions.pop().map(|p| p.join(r"x64\rc.exe"))
}
