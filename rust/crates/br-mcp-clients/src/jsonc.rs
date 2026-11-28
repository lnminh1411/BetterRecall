// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Lossless JSON and JSONC editing via `jsonc-parser`'s CST so comments, key order, and
//! formatting of unrelated entries survive untouched.

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstObjectProp, CstRootNode};

use crate::{ChangeKind, ConfigFormat, EntryState, SERVER_NAME};

fn parse_options(format: ConfigFormat) -> ParseOptions {
    let mut opts = ParseOptions::default();
    match format {
        ConfigFormat::Json | ConfigFormat::Toml => {
            opts.allow_comments = false;
            opts.allow_loose_object_property_names = false;
            opts.allow_trailing_commas = false;
        }
        ConfigFormat::Jsonc => {
            opts.allow_comments = true;
            opts.allow_loose_object_property_names = false;
            opts.allow_trailing_commas = true;
        }
    }
    opts
}

/// String value for `jsonc-parser`: its CST writer (0.26) escapes only `"`, so every other JSON escape
/// (backslashes in Windows paths, control characters) is applied here and the quote is left to it.
fn cst_string(s: &str) -> CstInputValue {
    let lit = json_string_literal(s);
    let body = &lit[1..lit.len() - 1];
    CstInputValue::String(body.replace("\\\"", "\""))
}

fn build_entry_value(
    extra_kv: Option<(&str, &str)>,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> CstInputValue {
    let mut fields = Vec::with_capacity(4);
    if let Some((k, v)) = extra_kv {
        fields.push((k.to_string(), cst_string(v)));
    }
    fields.push(("command".to_string(), cst_string(relay_str)));
    fields.push(("args".to_string(), CstInputValue::Array(Vec::new())));
    if let Some((ek, ev)) = server_env {
        fields.push((
            "env".to_string(),
            CstInputValue::Object(vec![(ek.to_string(), cst_string(ev))]),
        ));
    }
    CstInputValue::Object(fields)
}

fn string_prop_equals(obj: &CstObject, key: &str, expected: &str) -> bool {
    obj.get(key)
        .and_then(|p| p.value())
        .and_then(|v| v.as_string_lit())
        .and_then(|s| s.decoded_value().ok())
        .is_some_and(|v| v == expected)
}

fn empty_array_prop(obj: &CstObject, key: &str) -> bool {
    obj.get(key)
        .and_then(|p| p.array_value())
        .is_some_and(|arr| arr.elements().is_empty())
}

fn set_or_append_prop(obj: &CstObject, key: &str, val: CstInputValue) {
    if let Some(prop) = obj.get(key) {
        prop.set_value(val);
    } else {
        obj.append(key, val);
    }
}

fn update_entry_object(
    obj: &CstObject,
    extra_kv: Option<(&str, &str)>,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) {
    if let Some((k, v)) = extra_kv
        && !string_prop_equals(obj, k, v)
    {
        set_or_append_prop(obj, k, cst_string(v));
    }
    if !string_prop_equals(obj, "command", relay_str) {
        set_or_append_prop(obj, "command", cst_string(relay_str));
    }
    if !empty_array_prop(obj, "args") {
        set_or_append_prop(obj, "args", CstInputValue::Array(Vec::new()));
    }
    if let Some((ek, ev)) = server_env {
        match obj.get("env") {
            Some(env_prop) => {
                if let Some(env_obj) = env_prop.object_value() {
                    if !string_prop_equals(&env_obj, ek, ev) {
                        set_or_append_prop(&env_obj, ek, cst_string(ev));
                    }
                } else {
                    env_prop.set_value(CstInputValue::Object(vec![(
                        ek.to_string(),
                        cst_string(ev),
                    )]));
                }
            }
            None => {
                obj.append(
                    "env",
                    CstInputValue::Object(vec![(ek.to_string(), cst_string(ev))]),
                );
            }
        }
    }
}

fn extract_command(prop: &CstObjectProp) -> Option<String> {
    prop.object_value()?
        .get("command")?
        .value()?
        .as_string_lit()?
        .decoded_value()
        .ok()
}

fn entry_matches(
    prop: &CstObjectProp,
    extra_kv: Option<(&str, &str)>,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> bool {
    let Some(obj) = prop.object_value() else {
        return false;
    };
    if !string_prop_equals(&obj, "command", relay_str) {
        return false;
    }
    if !empty_array_prop(&obj, "args") {
        return false;
    }
    if let Some((k, expected_val)) = extra_kv
        && !string_prop_equals(&obj, k, expected_val)
    {
        return false;
    }
    if let Some((ek, ev)) = server_env {
        let Some(env_obj) = obj.get("env").and_then(|p| p.object_value()) else {
            return false;
        };
        if !string_prop_equals(&env_obj, ek, ev) {
            return false;
        }
    }
    true
}

/// Inspects an existing JSON/JSONC document for the `betterrecall` entry.
pub(crate) fn inspect_json(
    text: &str,
    format: ConfigFormat,
    servers_key: &str,
    extra_kv: Option<(&str, &str)>,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> Result<EntryState, String> {
    let root = CstRootNode::parse(text, &parse_options(format)).map_err(|e| e.to_string())?;
    let Some(root_obj) = root.object_value() else {
        return Err("root JSON value must be an object".to_string());
    };
    let Some(servers_prop) = root_obj.get(servers_key) else {
        return Ok(EntryState::Absent);
    };
    let Some(servers_obj) = servers_prop.object_value() else {
        return Err(format!("'{servers_key}' must be a JSON object"));
    };
    let Some(entry_prop) = servers_obj.get(SERVER_NAME) else {
        return Ok(EntryState::Absent);
    };
    let command = extract_command(&entry_prop);
    let matches = entry_matches(&entry_prop, extra_kv, relay_str, server_env);
    Ok(EntryState::Present { command, matches })
}

/// Adds or updates the `betterrecall` entry in a JSON/JSONC document.
/// Returns `(ChangeKind, Some(updated_text))` when a write is required, or
/// `(ChangeKind::Unchanged, None)` when an identical entry is already present.
pub(crate) fn add_json(
    existing: Option<&str>,
    format: ConfigFormat,
    servers_key: &str,
    extra_kv: Option<(&str, &str)>,
    needs_schema_version: bool,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> Result<(ChangeKind, Option<String>), String> {
    let Some(text) = existing else {
        let fresh = render_new_json(
            servers_key,
            extra_kv,
            needs_schema_version,
            relay_str,
            server_env,
        );
        return Ok((ChangeKind::Added, Some(fresh)));
    };

    let root = CstRootNode::parse(text, &parse_options(format)).map_err(|e| e.to_string())?;
    let Some(root_obj) = root.object_value() else {
        return Err("root JSON value must be an object".to_string());
    };

    let change = match root_obj.get(servers_key) {
        Some(servers_prop) => {
            let Some(servers_obj) = servers_prop.object_value() else {
                return Err(format!("'{servers_key}' must be a JSON object"));
            };
            match servers_obj.get(SERVER_NAME) {
                Some(entry_prop) => {
                    if entry_matches(&entry_prop, extra_kv, relay_str, server_env) {
                        return Ok((ChangeKind::Unchanged, None));
                    }
                    if let Some(entry_obj) = entry_prop.object_value() {
                        update_entry_object(&entry_obj, extra_kv, relay_str, server_env);
                    } else {
                        entry_prop.set_value(build_entry_value(extra_kv, relay_str, server_env));
                    }
                    ChangeKind::Updated
                }
                None => {
                    servers_obj.append(
                        SERVER_NAME,
                        build_entry_value(extra_kv, relay_str, server_env),
                    );
                    ChangeKind::Added
                }
            }
        }
        None => {
            let entry = build_entry_value(extra_kv, relay_str, server_env);
            root_obj.append(
                servers_key,
                CstInputValue::Object(vec![(SERVER_NAME.to_string(), entry)]),
            );
            ChangeKind::Added
        }
    };

    Ok((change, Some(root.to_string())))
}

/// Removes the `betterrecall` entry from an existing JSON/JSONC document.
/// Leaves an empty servers object if `betterrecall` was the last entry.
pub(crate) fn remove_json(
    text: &str,
    format: ConfigFormat,
    servers_key: &str,
) -> Result<(ChangeKind, Option<String>), String> {
    let root = CstRootNode::parse(text, &parse_options(format)).map_err(|e| e.to_string())?;
    let Some(root_obj) = root.object_value() else {
        return Err("root JSON value must be an object".to_string());
    };
    let Some(servers_prop) = root_obj.get(servers_key) else {
        return Ok((ChangeKind::NotPresent, None));
    };
    let Some(servers_obj) = servers_prop.object_value() else {
        return Err(format!("'{servers_key}' must be a JSON object"));
    };
    let Some(entry_prop) = servers_obj.get(SERVER_NAME) else {
        return Ok((ChangeKind::NotPresent, None));
    };
    entry_prop.remove();
    Ok((ChangeKind::Removed, Some(root.to_string())))
}

/// Escapes `s` as an RFC 8259 JSON string literal (including surrounding quotes).
pub(crate) fn json_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(&mut out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Renders a brand-new pretty-printed JSON config file with just the `betterrecall` entry
/// (and `"schema_version": 1` when required by the client).
pub(crate) fn render_new_json(
    servers_key: &str,
    extra_kv: Option<(&str, &str)>,
    needs_schema_version: bool,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> String {
    let escaped_cmd = json_string_literal(relay_str);
    let mut out = String::new();
    out.push_str("{\n");
    if needs_schema_version {
        out.push_str("  \"schema_version\": 1,\n");
    }
    out.push_str("  \"");
    out.push_str(servers_key);
    out.push_str("\": {\n");
    out.push_str("    \"");
    out.push_str(SERVER_NAME);
    out.push_str("\": {\n");
    if let Some((k, v)) = extra_kv {
        out.push_str("      \"");
        out.push_str(k);
        out.push_str("\": ");
        out.push_str(&json_string_literal(v));
        out.push_str(",\n");
    }
    out.push_str("      \"command\": ");
    out.push_str(&escaped_cmd);
    out.push_str(",\n");
    if let Some((ek, ev)) = server_env {
        out.push_str("      \"args\": [],\n");
        out.push_str("      \"env\": {\n");
        out.push_str("        ");
        out.push_str(&json_string_literal(ek));
        out.push_str(": ");
        out.push_str(&json_string_literal(ev));
        out.push_str("\n      }\n");
    } else {
        out.push_str("      \"args\": []\n");
    }
    out.push_str("    }\n");
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}
