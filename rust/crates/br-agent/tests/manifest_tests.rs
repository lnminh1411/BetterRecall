//! The agent exe must embed `app.manifest`: without the Windows 10 `supportedOS` entry and PerMonitorV2 DPI
//! awareness, DXGI Desktop Duplication fails and every output stays unavailable (found in the A7 measurements).

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

#[test]
fn manifest_copy_matches_the_csharp_agent() {
    let rust = include_str!("../app.manifest");
    let cs = include_str!("../../../../src/BetterRecall.Agent/app.manifest");
    assert_eq!(cs.replace("\r\n", "\n"), rust.replace("\r\n", "\n"));
}
