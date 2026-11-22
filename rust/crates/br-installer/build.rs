//! Embeds the installer's Win32 resources into the exe: the application icon
//! (`assets\icons\app.ico`), version information, and `app.manifest` (with `asInvoker` so Windows'
//! installer detection does not elevate `BetterRecall-Setup.exe`).
//!
//! Compiled with the Windows SDK's `rc.exe` (from `RC` or the newest `Windows Kits\10\bin\<version>\x64\rc.exe`)
//! and linked with `/MANIFEST:NO`.

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
    let rc_path = out.join("installer.rc");
    std::fs::write(&rc_path, resource_script(&version, &icon, &manifest)).unwrap();
    let res_path = out.join("installer.res");
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
      VALUE "FileDescription", "BetterRecall Setup"
      VALUE "FileVersion", "{dotted}"
      VALUE "InternalName", "BetterRecall Setup"
      VALUE "LegalCopyright", "BetterRecall contributors, AGPL-3.0-only"
      VALUE "OriginalFilename", "BetterRecall-Setup.exe"
      VALUE "ProductName", "BetterRecall"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0, 1200
  END
END

101 DIALOGEX 0, 0, 317, 165
STYLE 0x40000040
FONT 9, "Segoe UI"
BEGIN
  LTEXT "", 1001, 0, 0, 317, 18, 0x0080
  EDITTEXT 1002, 0, 22, 317, 104, 0x50A10844
  LTEXT "", 1003, 0, 130, 317, 14, 0x0080
  AUTOCHECKBOX "", 1004, 0, 148, 317, 12, 0x50010003
END

102 DIALOGEX 0, 0, 317, 165
STYLE 0x40000040
FONT 9, "Segoe UI"
BEGIN
  AUTORADIOBUTTON "", 1101, 0, 4, 317, 12, 0x50030009
  LTEXT "", 1102, 14, 18, 303, 18, 0x0080
  AUTORADIOBUTTON "", 1103, 0, 42, 317, 12, 0x50010009
  LTEXT "", 1104, 14, 56, 303, 18, 0x0080
  AUTORADIOBUTTON "", 1105, 0, 80, 317, 12, 0x50010009
  LTEXT "", 1106, 14, 94, 303, 28, 0x0080
  LTEXT "", 1107, 0, 134, 317, 20, 0x0080
END

103 DIALOGEX 0, 0, 317, 165
STYLE 0x40000040
FONT 9, "Segoe UI"
BEGIN
  LTEXT "", 1201, 0, 6, 317, 10, 0x0080
  EDITTEXT 1202, 0, 18, 317, 14, 0x50810880
  LTEXT "", 1203, 0, 42, 317, 10, 0x0080
  EDITTEXT 1204, 0, 54, 317, 14, 0x50810880
  AUTOCHECKBOX "", 1205, 0, 82, 317, 12, 0x50010003
END

104 DIALOGEX 0, 0, 317, 165
STYLE 0x40000040
FONT 9, "Segoe UI"
BEGIN
  AUTORADIOBUTTON "", 1301, 0, 6, 317, 22, 0x50032009
  AUTORADIOBUTTON "", 1302, 0, 34, 317, 12, 0x50010009
  LTEXT "", 1303, 14, 50, 303, 28, 0x0080
  AUTOCHECKBOX "", 1304, 14, 84, 303, 12, 0x50010003
END

105 DIALOGEX 0, 0, 317, 165
STYLE 0x40000040
FONT 9, "Segoe UI"
BEGIN
  LTEXT "", 1401, 0, 14, 317, 14, 0x0080
  CONTROL "", 1402, "msctls_progress32", 0x50000001, 0, 34, 317, 14
END

106 DIALOGEX 0, 0, 317, 165
STYLE 0x40000040
FONT 9, "Segoe UI"
BEGIN
  LTEXT "", 1501, 0, 8, 317, 32, 0x0080
  LTEXT "", 1502, 0, 46, 317, 96, 0x0080
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
