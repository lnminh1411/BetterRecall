//! The agent exe must embed its Win32 resources (`build.rs`): `app.manifest`, the icon and version information.
//! Without the manifest's Windows 10 `supportedOS` entry and PerMonitorV2 DPI awareness, DXGI Desktop Duplication
//! fails and every output stays unavailable (found in the A7 measurements).

fn exe_text() -> String {
    let bytes = std::fs::read(env!("CARGO_BIN_EXE_br-agent")).expect("agent exe");
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn agent_exe_embeds_the_windows10_and_per_monitor_v2_manifest() {
    let text = exe_text();
    assert!(
        text.contains("{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"),
        "supportedOS Windows 10 entry missing"
    );
    assert!(
        text.contains("PerMonitorV2"),
        "dpiAwareness PerMonitorV2 missing"
    );
    assert!(
        text.contains("Microsoft.Windows.Common-Controls"),
        "common controls v6 dependency missing"
    );
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn agent_exe_embeds_the_app_icon_and_version_info() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_br-agent")).expect("agent exe");
    assert!(
        contains(&exe, &utf16("VS_VERSION_INFO")),
        "version resource missing"
    );
    assert!(
        contains(&exe, &utf16("BetterRecall.Agent.exe")),
        "OriginalFilename missing"
    );
    assert!(
        contains(&exe, &utf16(env!("CARGO_PKG_VERSION"))),
        "ProductVersion missing"
    );
    // The first image of app.ico (ICONDIRENTRY: size at +8, offset at +12) is stored as an RT_ICON resource.
    let ico = include_bytes!("../../../../assets/icons/app.ico");
    let size = u32::from_le_bytes(ico[14..18].try_into().unwrap()) as usize;
    let offset = u32::from_le_bytes(ico[18..22].try_into().unwrap()) as usize;
    let image = &ico[offset..offset + size.min(256)];
    assert!(contains(&exe, image), "app.ico image missing");
}
