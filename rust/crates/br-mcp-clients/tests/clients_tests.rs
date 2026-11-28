// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Integration tests for `br-mcp-clients`. All tests run in isolated temp directories under
//! `BR_TEST_TMP` and never touch the real user profile.

use std::fs;
use std::path::{Path, PathBuf};

use br_mcp_clients::{
    ChangeKind, ClientEnv, ClientError, ClientKind, EntryState, add, list, remove, snippet,
};
use tempfile::TempDir;

fn test_tmp_root() -> PathBuf {
    if let Some(t) = std::env::var_os("BR_TEST_TMP")
        && !t.is_empty()
    {
        return PathBuf::from(t);
    }
    if let Some(t) = std::env::var_os("CARGO_TARGET_DIR")
        && !t.is_empty()
    {
        return PathBuf::from(t).join("test-tmp");
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| d.parent()).map(|d| d.join("test-tmp")))
        .expect("unable to determine cargo target dir for test temp folder")
}

fn new_test_dir(prefix: &str) -> TempDir {
    let base = test_tmp_root();
    fs::create_dir_all(&base).expect("create BR_TEST_TMP root");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&base)
        .expect("create test tempdir")
}

fn env_in(root: &Path) -> ClientEnv {
    ClientEnv {
        home: root.join("home"),
        appdata: root.join("appdata"),
        xdg_config_home: None,
        codex_home: None,
    }
}

fn sample_relay() -> PathBuf {
    PathBuf::from(r"K:\Fake Program Files\Better Recall\app\bin\betterrecall-mcp.exe")
}

fn assert_no_temp_files(dir: &Path) {
    if !dir.is_dir() {
        return;
    }
    for entry in fs::read_dir(dir).expect("read_dir").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".betterrecall-tmp-"),
            "leftover temp file found: {}",
            entry.path().display()
        );
    }
}

const ALL_WRITABLE: &[&str] = &[
    "claude-desktop",
    "antigravity",
    "gemini-cli",
    "cursor",
    "windsurf",
    "vscode",
    "muse-code",
    "codex",
];

fn detect_dir_for(env: &ClientEnv, id: &str) -> PathBuf {
    match id {
        "claude-desktop" => env.appdata.join("Claude"),
        "antigravity" => env.home.join(".gemini").join("antigravity-cli"),
        "gemini-cli" => env.home.join(".gemini"),
        "cursor" => env.home.join(".cursor"),
        "windsurf" => env.home.join(".codeium").join("windsurf"),
        "vscode" => env.appdata.join("Code").join("User"),
        "muse-code" => env
            .xdg_config_home
            .clone()
            .unwrap_or_else(|| env.home.join(".config"))
            .join("muse"),
        "codex" => env
            .codex_home
            .clone()
            .unwrap_or_else(|| env.home.join(".codex")),
        other => panic!("unexpected writable client {other}"),
    }
}

#[test]
fn all_writable_clients_missing_file_lifecycle() {
    let relay = sample_relay();
    let relay_str = relay.to_string_lossy().into_owned();

    for &id in ALL_WRITABLE {
        let tmp = new_test_dir("br-mcp-life-");
        let env = env_in(tmp.path());

        // Undetected: add fails with NotDetected and creates nothing.
        let err = add(&env, id, &relay).unwrap_err();
        assert!(
            matches!(err, ClientError::NotDetected(_)),
            "{id}: expected NotDetected, got {err:?}"
        );

        // Remove on missing file -> NotPresent, no write.
        let rem = remove(&env, id).expect("remove on missing file");
        assert_eq!(rem.kind, ChangeKind::NotPresent, "{id}");
        assert!(rem.backup.is_none(), "{id}");
        assert!(!rem.path.exists(), "{id}");

        // Create detection folder (for antigravity, parent `.gemini\config` still doesn't exist!).
        fs::create_dir_all(detect_dir_for(&env, id)).expect("create detect dir");

        // Add into missing file -> Added, no backup.
        let added = add(&env, id, &relay).expect("add into missing file");
        assert_eq!(added.kind, ChangeKind::Added, "{id}");
        assert!(added.backup.is_none(), "{id}");
        assert!(added.path.is_file(), "{id}");
        assert_no_temp_files(added.path.parent().expect("parent"));

        // list reports detected + Present + matches: true.
        let statuses = list(&env, &relay);
        let st = statuses.iter().find(|s| s.id == id).expect("client in list");
        assert!(st.detected, "{id}");
        assert_eq!(
            st.state,
            EntryState::Present {
                command: Some(relay_str.clone()),
                matches: true
            },
            "{id}"
        );

        // Add twice -> Unchanged, no backup.
        let again = add(&env, id, &relay).expect("second add");
        assert_eq!(again.kind, ChangeKind::Unchanged, "{id}");
        assert!(again.backup.is_none(), "{id}");

        // Remove -> Removed, backup created, file still exists with empty servers container.
        let removed = remove(&env, id).expect("remove");
        assert_eq!(removed.kind, ChangeKind::Removed, "{id}");
        assert!(removed.backup.as_ref().is_some_and(|b| b.is_file()), "{id}");
        assert!(removed.path.is_file(), "{id}");

        // list now reports Absent.
        let statuses_after = list(&env, &relay);
        let st_after = statuses_after
            .iter()
            .find(|s| s.id == id)
            .expect("client in list");
        assert_eq!(st_after.state, EntryState::Absent, "{id}");

        // Verify the servers key/table itself was NOT deleted.
        let content_after = fs::read_to_string(&removed.path).expect("read after remove");
        let expected_key = match id {
            "vscode" => "\"servers\"",
            "muse-code" => "\"mcp_servers\"",
            "codex" => "[mcp_servers]",
            _ => "\"mcpServers\"",
        };
        assert!(
            content_after.contains(expected_key),
            "{id}: expected empty servers key {expected_key} to remain in:\n{content_after}"
        );

        // Second remove -> NotPresent, no new write.
        let rem2 = remove(&env, id).expect("second remove");
        assert_eq!(rem2.kind, ChangeKind::NotPresent, "{id}");
        assert!(rem2.backup.is_none(), "{id}");
    }
}

#[test]
fn json_preserves_other_servers_unknown_keys_and_order() {
    let tmp = new_test_dir("br-mcp-json-");
    let env = env_in(tmp.path());
    let relay = sample_relay();

    let dir = env.home.join(".cursor");
    fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("mcp.json");
    let original = concat!(
        "{\n",
        "  \"theme\": \"dark\",\n",
        "  \"mcpServers\": {\n",
        "    \"alpha\": {\n",
        "      \"command\": \"C:\\\\tools\\\\alpha.exe\",\n",
        "      \"args\": [\"--stdio\"]\n",
        "    }\n",
        "  },\n",
        "  \"trailingKey\": 42\n",
        "}\n"
    );
    fs::write(&cfg, original).unwrap();

    let res = add(&env, "cursor", &relay).expect("add cursor");
    assert_eq!(res.kind, ChangeKind::Added);
    let bak = res.backup.expect("backup created");
    assert_eq!(fs::read_to_string(&bak).unwrap(), original);

    let updated = fs::read_to_string(&cfg).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&updated).expect("valid json");
    assert_eq!(parsed["theme"], "dark");
    assert_eq!(parsed["trailingKey"], 42);
    assert_eq!(parsed["mcpServers"]["alpha"]["command"], r"C:\tools\alpha.exe");
    assert_eq!(parsed["mcpServers"]["alpha"]["args"][0], "--stdio");
    assert_eq!(
        parsed["mcpServers"]["betterrecall"]["command"],
        relay.to_string_lossy().as_ref()
    );
    assert_eq!(parsed["mcpServers"]["betterrecall"]["args"], serde_json::json!([]));

    // Key order preserved: theme before mcpServers before trailingKey.
    let i_theme = updated.find("\"theme\"").unwrap();
    let i_servers = updated.find("\"mcpServers\"").unwrap();
    let i_trailing = updated.find("\"trailingKey\"").unwrap();
    assert!(i_theme < i_servers && i_servers < i_trailing);

    // Remove leaves alpha and unknown keys intact.
    let rem = remove(&env, "cursor").expect("remove cursor");
    assert_eq!(rem.kind, ChangeKind::Removed);
    let after_rem = fs::read_to_string(&cfg).unwrap();
    let parsed_rem: serde_json::Value = serde_json::from_str(&after_rem).expect("valid json");
    assert_eq!(parsed_rem["theme"], "dark");
    assert_eq!(parsed_rem["trailingKey"], 42);
    assert_eq!(parsed_rem["mcpServers"]["alpha"]["command"], r"C:\tools\alpha.exe");
    assert!(parsed_rem["mcpServers"].get("betterrecall").is_none());
}

#[test]
fn vscode_jsonc_preserves_comments_and_other_servers() {
    let tmp = new_test_dir("br-mcp-vscode-");
    let env = env_in(tmp.path());
    let relay = sample_relay();

    let dir = env.appdata.join("Code").join("User");
    fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("mcp.json");
    let original = concat!(
        "// top-level comment\n",
        "{\n",
        "  /* block comment */\n",
        "  \"servers\": {\n",
        "    // existing server comment\n",
        "    \"other\": {\n",
        "      \"type\": \"stdio\",\n",
        "      \"command\": \"other-mcp\"\n",
        "    }\n",
        "  },\n",
        "  \"inputs\": []\n",
        "}\n"
    );
    fs::write(&cfg, original).unwrap();

    let res = add(&env, "vscode", &relay).expect("add vscode");
    assert_eq!(res.kind, ChangeKind::Added);
    let text = fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("// top-level comment"));
    assert!(text.contains("/* block comment */"));
    assert!(text.contains("// existing server comment"));
    assert!(text.contains("\"other\""));
    assert!(text.contains("\"type\": \"stdio\""));
    assert!(text.contains("\"inputs\": []"));

    // Verify list sees it as Present + matches.
    let st = list(&env, &relay)
        .into_iter()
        .find(|s| s.id == "vscode")
        .unwrap();
    assert_eq!(
        st.state,
        EntryState::Present {
            command: Some(relay.to_string_lossy().into_owned()),
            matches: true,
        }
    );

    // Remove keeps comments and other server.
    let rem = remove(&env, "vscode").expect("remove vscode");
    assert_eq!(rem.kind, ChangeKind::Removed);
    let after = fs::read_to_string(&cfg).unwrap();
    assert!(after.contains("// top-level comment"));
    assert!(after.contains("/* block comment */"));
    assert!(after.contains("// existing server comment"));
    assert!(after.contains("\"other\""));
    assert!(!after.contains("\"betterrecall\""));
}

#[test]
fn codex_toml_preserves_comments_unknown_keys_and_codex_home() {
    let tmp = new_test_dir("br-mcp-codex-");
    let mut env = env_in(tmp.path());
    let custom_codex = tmp.path().join("custom-codex");
    fs::create_dir_all(&custom_codex).unwrap();
    env.codex_home = Some(custom_codex.clone());

    let relay = sample_relay();
    let cfg = custom_codex.join("config.toml");
    let original = concat!(
        "# Codex config header\n",
        "model = \"o3\"\n",
        "\n",
        "[mcp_servers.existing]\n",
        "# keep this comment\n",
        "command = \"C:\\\\bin\\\\existing.exe\"\n",
        "args = [\"--flag\"]\n"
    );
    fs::write(&cfg, original).unwrap();

    let res = add(&env, "codex", &relay).expect("add codex");
    assert_eq!(res.kind, ChangeKind::Added);
    assert_eq!(res.path, cfg);
    assert_eq!(fs::read_to_string(res.backup.unwrap()).unwrap(), original);

    let text = fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("# Codex config header"));
    assert!(text.contains("model = \"o3\""));
    assert!(text.contains("# keep this comment"));
    assert!(text.contains("[mcp_servers.existing]"));
    assert!(text.contains("[mcp_servers.betterrecall]"));

    // Verify round-trip of relay path with spaces and backslashes.
    let st = list(&env, &relay)
        .into_iter()
        .find(|s| s.id == "codex")
        .unwrap();
    assert_eq!(
        st.state,
        EntryState::Present {
            command: Some(relay.to_string_lossy().into_owned()),
            matches: true,
        }
    );

    // Second add is Unchanged.
    assert_eq!(add(&env, "codex", &relay).unwrap().kind, ChangeKind::Unchanged);

    // Remove leaves existing server and comments untouched.
    let rem = remove(&env, "codex").expect("remove codex");
    assert_eq!(rem.kind, ChangeKind::Removed);
    let after = fs::read_to_string(&cfg).unwrap();
    assert!(after.contains("# Codex config header"));
    assert!(after.contains("model = \"o3\""));
    assert!(after.contains("# keep this comment"));
    assert!(after.contains("[mcp_servers.existing]"));
    assert!(!after.contains("betterrecall"));
}

#[test]
fn muse_code_schema_version_and_xdg_config_home() {
    let tmp = new_test_dir("br-mcp-muse-");
    let mut env = env_in(tmp.path());
    let xdg = tmp.path().join("xdg");
    let muse_dir = xdg.join("muse");
    fs::create_dir_all(&muse_dir).unwrap();
    env.xdg_config_home = Some(xdg);

    let relay = sample_relay();

    // New file gets schema_version: 1.
    let res = add(&env, "muse-code", &relay).expect("add muse-code");
    assert_eq!(res.kind, ChangeKind::Added);
    let created: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&res.path).unwrap()).unwrap();
    assert_eq!(created["schema_version"], 1);
    assert_eq!(created["mcp_servers"]["betterrecall"]["transport"], "stdio");
    assert_eq!(
        created["mcp_servers"]["betterrecall"]["command"],
        relay.to_string_lossy().as_ref()
    );
    assert_eq!(
        created["mcp_servers"]["betterrecall"]["args"],
        serde_json::json!([])
    );

    // Existing file with custom schema_version: 2 must never be changed.
    let existing = "{\n  \"schema_version\": 2,\n  \"other\": true\n}\n";
    fs::write(&res.path, existing).unwrap();
    let res2 = add(&env, "muse-code", &relay).expect("add into existing muse-code");
    assert_eq!(res2.kind, ChangeKind::Added);
    let updated: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&res.path).unwrap()).unwrap();
    assert_eq!(updated["schema_version"], 2);
    assert_eq!(updated["other"], true);
    assert_eq!(updated["mcp_servers"]["betterrecall"]["transport"], "stdio");
}

#[test]
fn bom_and_crlf_preserved_json_and_toml() {
    let tmp = new_test_dir("br-mcp-bom-");
    let env = env_in(tmp.path());
    let relay = sample_relay();

    // JSON with UTF-8 BOM + CRLF.
    let cursor_dir = env.home.join(".cursor");
    fs::create_dir_all(&cursor_dir).unwrap();
    let cursor_cfg = cursor_dir.join("mcp.json");
    let mut json_bytes = b"\xEF\xBB\xBF".to_vec();
    json_bytes.extend_from_slice(b"{\r\n  \"mcpServers\": {}\r\n}\r\n");
    fs::write(&cursor_cfg, &json_bytes).unwrap();

    add(&env, "cursor", &relay).expect("add cursor bom+crlf");
    let out_json = fs::read(&cursor_cfg).unwrap();
    assert!(out_json.starts_with(b"\xEF\xBB\xBF"), "JSON BOM lost");
    let out_json_str = std::str::from_utf8(&out_json[3..]).unwrap();
    assert!(out_json_str.contains("\r\n"), "JSON CRLF lost");
    assert!(
        !out_json_str.replace("\r\n", "").contains('\n'),
        "JSON introduced bare LF"
    );

    // TOML with UTF-8 BOM + CRLF.
    let codex_dir = env.home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let codex_cfg = codex_dir.join("config.toml");
    let mut toml_bytes = b"\xEF\xBB\xBF".to_vec();
    toml_bytes.extend_from_slice(b"model = \"o3\"\r\n");
    fs::write(&codex_cfg, &toml_bytes).unwrap();

    add(&env, "codex", &relay).expect("add codex bom+crlf");
    let out_toml = fs::read(&codex_cfg).unwrap();
    assert!(out_toml.starts_with(b"\xEF\xBB\xBF"), "TOML BOM lost");
    let out_toml_str = std::str::from_utf8(&out_toml[3..]).unwrap();
    assert!(out_toml_str.contains("\r\n"), "TOML CRLF lost");
    assert!(
        !out_toml_str.replace("\r\n", "").contains('\n'),
        "TOML introduced bare LF"
    );
}

#[test]
fn malformed_files_return_malformed_untouched_no_backup_no_temp() {
    let tmp = new_test_dir("br-mcp-bad-");
    let env = env_in(tmp.path());
    let relay = sample_relay();

    // 1. Broken JSON syntax
    let cursor_dir = env.home.join(".cursor");
    fs::create_dir_all(&cursor_dir).unwrap();
    let cursor_cfg = cursor_dir.join("mcp.json");
    let broken_json = "{ not valid json";
    fs::write(&cursor_cfg, broken_json).unwrap();

    let err = add(&env, "cursor", &relay).unwrap_err();
    assert!(matches!(err, ClientError::Malformed { .. }));
    assert_eq!(fs::read_to_string(&cursor_cfg).unwrap(), broken_json);
    assert!(!cursor_dir.join("mcp.json.bak-betterrecall").exists());
    assert_no_temp_files(&cursor_dir);

    let st = list(&env, &relay)
        .into_iter()
        .find(|s| s.id == "cursor")
        .unwrap();
    assert!(matches!(st.state, EntryState::Unreadable(_)));

    // 2. Servers key is not an object
    let bad_servers = r#"{"mcpServers": ["not", "an", "object"]}"#;
    fs::write(&cursor_cfg, bad_servers).unwrap();
    assert!(matches!(
        add(&env, "cursor", &relay),
        Err(ClientError::Malformed { .. })
    ));
    assert!(matches!(
        remove(&env, "cursor"),
        Err(ClientError::Malformed { .. })
    ));
    assert_eq!(fs::read_to_string(&cursor_cfg).unwrap(), bad_servers);
    assert!(!cursor_dir.join("mcp.json.bak-betterrecall").exists());
    assert_no_temp_files(&cursor_dir);

    // 3. TOML servers key is not a table
    let codex_dir = env.home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let codex_cfg = codex_dir.join("config.toml");
    let bad_toml = "mcp_servers = \"nope\"\n";
    fs::write(&codex_cfg, bad_toml).unwrap();
    assert!(matches!(
        add(&env, "codex", &relay),
        Err(ClientError::Malformed { .. })
    ));
    assert!(matches!(
        remove(&env, "codex"),
        Err(ClientError::Malformed { .. })
    ));
    assert_eq!(fs::read_to_string(&codex_cfg).unwrap(), bad_toml);
    assert!(!codex_dir.join("config.toml.bak-betterrecall").exists());
    assert_no_temp_files(&codex_dir);
}

#[test]
fn stale_entry_reports_matches_false_and_add_updates_it() {
    let tmp = new_test_dir("br-mcp-stale-");
    let env = env_in(tmp.path());
    let old_relay = PathBuf::from(r"C:\Old\betterrecall-mcp.exe");
    let new_relay = sample_relay();

    let dir = env.appdata.join("Claude");
    fs::create_dir_all(&dir).unwrap();
    add(&env, "claude-desktop", &old_relay).expect("add old");

    let st = list(&env, &new_relay)
        .into_iter()
        .find(|s| s.id == "claude-desktop")
        .unwrap();
    assert_eq!(
        st.state,
        EntryState::Present {
            command: Some(old_relay.to_string_lossy().into_owned()),
            matches: false,
        }
    );

    let upd = add(&env, "claude-desktop", &new_relay).expect("update");
    assert_eq!(upd.kind, ChangeKind::Updated);
    assert!(upd.backup.is_some());

    let st2 = list(&env, &new_relay)
        .into_iter()
        .find(|s| s.id == "claude-desktop")
        .unwrap();
    assert_eq!(
        st2.state,
        EntryState::Present {
            command: Some(new_relay.to_string_lossy().into_owned()),
            matches: true,
        }
    );
}

#[test]
fn snippet_clients_detection_errors_and_content() {
    let tmp = new_test_dir("br-mcp-snip-");
    let env = env_in(tmp.path());
    let relay = sample_relay();
    let relay_str = relay.to_string_lossy().into_owned();

    // Initially none detected.
    for st in list(&env, &relay) {
        assert!(!st.detected, "{} should not be detected", st.id);
        if matches!(st.kind, ClientKind::Snippet) {
            assert_eq!(st.state, EntryState::Unknown);
        }
    }

    // Create detection markers for claude-code and deepseek-harness.
    fs::create_dir_all(&env.home).unwrap();
    fs::write(env.home.join(".claude.json"), "{}").unwrap();
    fs::create_dir_all(env.home.join(".dsh")).unwrap();

    let statuses = list(&env, &relay);
    assert!(statuses.iter().find(|s| s.id == "claude-code").unwrap().detected);
    assert!(statuses.iter().find(|s| s.id == "deepseek-harness").unwrap().detected);
    assert!(!statuses.iter().find(|s| s.id == "zed").unwrap().detected);
    assert!(!statuses.iter().find(|s| s.id == "cline").unwrap().detected);
    assert!(!statuses.iter().find(|s| s.id == "other").unwrap().detected);

    // add/remove on snippet clients return SnippetOnly.
    for id in ["claude-code", "deepseek-harness", "zed", "cline", "other"] {
        assert!(matches!(add(&env, id, &relay), Err(ClientError::SnippetOnly(_))));
        assert!(matches!(remove(&env, id), Err(ClientError::SnippetOnly(_))));
    }

    // Unknown id returns UnknownClient.
    assert!(matches!(
        add(&env, "nonexistent", &relay),
        Err(ClientError::UnknownClient(_))
    ));
    assert!(matches!(
        remove(&env, "nonexistent"),
        Err(ClientError::UnknownClient(_))
    ));
    assert!(matches!(
        snippet(&env, "nonexistent", &relay),
        Err(ClientError::UnknownClient(_))
    ));

    // Verify snippet contents.
    let cc = snippet(&env, "claude-code", &relay).unwrap();
    let cmd = cc.command.expect("claude-code has command");
    assert_eq!(
        cmd,
        format!("claude mcp add --scope user betterrecall -- \"{relay_str}\"")
    );
    assert!(cc.text.contains(&relay_str));

    let dsh = snippet(&env, "deepseek-harness", &relay).unwrap();
    assert!(dsh.command.is_none());
    assert!(dsh.text.contains("- insert:"));
    assert!(dsh.text.contains("@deepseek-ai/dsh-mcp-client"));
    assert!(dsh.text.contains("serverName: betterrecall"));
    assert!(dsh.text.contains("transport: stdio"));
    assert!(dsh.text.contains(&relay_str));
    assert!(dsh.text.to_lowercase().contains("merge"));
    assert!(dsh.target_hint.contains("cordis.patch.yml"));

    let zed = snippet(&env, "zed", &relay).unwrap();
    assert!(zed.text.contains("\"context_servers\""));
    assert!(zed.text.contains("\"betterrecall\""));
    assert!(zed.text.contains(&relay_str.replace('\\', "\\\\")));

    let cline = snippet(&env, "cline", &relay).unwrap();
    let cline_json: serde_json::Value = serde_json::from_str(&cline.text).expect("valid json");
    assert_eq!(cline_json["mcpServers"]["betterrecall"]["command"], relay_str);

    let other = snippet(&env, "other", &relay).unwrap();
    let other_json: serde_json::Value = serde_json::from_str(&other.text).expect("valid json");
    assert_eq!(other_json["mcpServers"]["betterrecall"]["command"], relay_str);
}
