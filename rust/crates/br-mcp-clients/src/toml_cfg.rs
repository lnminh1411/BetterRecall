// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Lossless TOML editing via `toml_edit` for Codex CLI's `config.toml`.

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use crate::{ChangeKind, EntryState, SERVER_NAME};

const SERVERS_KEY: &str = "mcp_servers";

fn build_env_inline_table(k: &str, v: &str) -> InlineTable {
    let mut env = InlineTable::new();
    env.insert(k, Value::from(v));
    env
}

fn env_item_matches(item: Option<&Item>, ek: &str, ev: &str) -> bool {
    let Some(item) = item else {
        return false;
    };
    if let Some(tbl) = item.as_table() {
        tbl.get(ek).and_then(Item::as_str) == Some(ev)
    } else if let Some(inline) = item.as_inline_table() {
        inline.get(ek).and_then(Value::as_str) == Some(ev)
    } else {
        false
    }
}

fn table_matches(tbl: &Table, relay_str: &str, server_env: Option<(&str, &str)>) -> bool {
    let cmd_ok = tbl.get("command").and_then(Item::as_str) == Some(relay_str);
    let args_ok = tbl
        .get("args")
        .and_then(Item::as_array)
        .is_some_and(Array::is_empty);
    let env_ok = match server_env {
        None => true,
        Some((ek, ev)) => env_item_matches(tbl.get("env"), ek, ev),
    };
    cmd_ok && args_ok && env_ok
}

fn inline_table_matches(
    tbl: &InlineTable,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> bool {
    let cmd_ok = tbl.get("command").and_then(Value::as_str) == Some(relay_str);
    let args_ok = tbl
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(Array::is_empty);
    let env_ok = match server_env {
        None => true,
        Some((ek, ev)) => tbl
            .get("env")
            .and_then(Value::as_inline_table)
            .and_then(|e| e.get(ek))
            .and_then(Value::as_str)
            == Some(ev),
    };
    cmd_ok && args_ok && env_ok
}

fn item_matches(item: &Item, relay_str: &str, server_env: Option<(&str, &str)>) -> bool {
    if let Some(tbl) = item.as_table() {
        table_matches(tbl, relay_str, server_env)
    } else if let Some(inline) = item.as_inline_table() {
        inline_table_matches(inline, relay_str, server_env)
    } else {
        false
    }
}

fn item_command(item: &Item) -> Option<String> {
    if let Some(tbl) = item.as_table() {
        tbl.get("command").and_then(Item::as_str).map(String::from)
    } else if let Some(inline) = item.as_inline_table() {
        inline
            .get("command")
            .and_then(Value::as_str)
            .map(String::from)
    } else {
        None
    }
}

fn value_command(val: &Value) -> Option<String> {
    val.as_inline_table()?
        .get("command")
        .and_then(Value::as_str)
        .map(String::from)
}

fn build_entry_table(relay_str: &str, server_env: Option<(&str, &str)>) -> Table {
    let mut entry = Table::new();
    entry.insert("command", toml_edit::value(relay_str));
    entry.insert("args", toml_edit::value(Array::new()));
    if let Some((ek, ev)) = server_env {
        entry.insert(
            "env",
            Item::Value(Value::InlineTable(build_env_inline_table(ek, ev))),
        );
    }
    entry
}

fn build_entry_inline_table(relay_str: &str, server_env: Option<(&str, &str)>) -> InlineTable {
    let mut entry = InlineTable::new();
    entry.insert("command", Value::from(relay_str));
    entry.insert("args", Value::Array(Array::new()));
    if let Some((ek, ev)) = server_env {
        entry.insert("env", Value::InlineTable(build_env_inline_table(ek, ev)));
    }
    entry
}

fn set_keep_key(tbl: &mut Table, key: &str, item: Item) {
    match tbl.get_mut(key) {
        Some(slot) => *slot = item,
        None => {
            tbl.insert(key, item);
        }
    }
}

fn update_entry_table(tbl: &mut Table, relay_str: &str, server_env: Option<(&str, &str)>) {
    // Replace the item, not the key: `Table::insert` makes a new key and drops comments above it.
    if tbl.get("command").and_then(Item::as_str) != Some(relay_str) {
        set_keep_key(tbl, "command", toml_edit::value(relay_str));
    }
    if !tbl
        .get("args")
        .and_then(Item::as_array)
        .is_some_and(Array::is_empty)
    {
        set_keep_key(tbl, "args", toml_edit::value(Array::new()));
    }
    if let Some((ek, ev)) = server_env {
        if let Some(env_item) = tbl.get_mut("env") {
            if let Some(env_tbl) = env_item.as_table_mut() {
                if env_tbl.get(ek).and_then(Item::as_str) != Some(ev) {
                    env_tbl.insert(ek, toml_edit::value(ev));
                }
            } else if let Some(env_inline) = env_item.as_inline_table_mut() {
                if env_inline.get(ek).and_then(Value::as_str) != Some(ev) {
                    env_inline.insert(ek, Value::from(ev));
                }
            } else {
                *env_item = Item::Value(Value::InlineTable(build_env_inline_table(ek, ev)));
            }
        } else {
            tbl.insert(
                "env",
                Item::Value(Value::InlineTable(build_env_inline_table(ek, ev))),
            );
        }
    }
}

fn update_entry_inline_table(
    tbl: &mut InlineTable,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) {
    if tbl.get("command").and_then(Value::as_str) != Some(relay_str) {
        tbl.insert("command", Value::from(relay_str));
    }
    if !tbl
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(Array::is_empty)
    {
        tbl.insert("args", Value::Array(Array::new()));
    }
    if let Some((ek, ev)) = server_env {
        if let Some(env_val) = tbl.get_mut("env") {
            if let Some(env_inline) = env_val.as_inline_table_mut() {
                if env_inline.get(ek).and_then(Value::as_str) != Some(ev) {
                    env_inline.insert(ek, Value::from(ev));
                }
            } else {
                *env_val = Value::InlineTable(build_env_inline_table(ek, ev));
            }
        } else {
            tbl.insert("env", Value::InlineTable(build_env_inline_table(ek, ev)));
        }
    }
}

/// Inspects an existing TOML document for `[mcp_servers.betterrecall]`.
pub(crate) fn inspect_toml(
    text: &str,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> Result<EntryState, String> {
    let doc = text.parse::<DocumentMut>().map_err(|e| e.to_string())?;
    let Some(servers_item) = doc.get(SERVERS_KEY) else {
        return Ok(EntryState::Absent);
    };
    if let Some(tbl) = servers_item.as_table() {
        let Some(entry) = tbl.get(SERVER_NAME) else {
            return Ok(EntryState::Absent);
        };
        Ok(EntryState::Present {
            command: item_command(entry),
            matches: item_matches(entry, relay_str, server_env),
        })
    } else if let Some(inline) = servers_item.as_inline_table() {
        let Some(entry) = inline.get(SERVER_NAME) else {
            return Ok(EntryState::Absent);
        };
        let matches = entry
            .as_inline_table()
            .is_some_and(|t| inline_table_matches(t, relay_str, server_env));
        Ok(EntryState::Present {
            command: value_command(entry),
            matches,
        })
    } else {
        Err(format!("'{SERVERS_KEY}' must be a TOML table"))
    }
}

/// Adds or updates `[mcp_servers.betterrecall]` in a TOML document.
pub(crate) fn add_toml(
    existing: Option<&str>,
    relay_str: &str,
    server_env: Option<(&str, &str)>,
) -> Result<(ChangeKind, Option<String>), String> {
    let Some(text) = existing else {
        return Ok((
            ChangeKind::Added,
            Some(render_new_toml(relay_str, server_env)),
        ));
    };

    let mut doc = text.parse::<DocumentMut>().map_err(|e| e.to_string())?;
    let change = match doc.get_mut(SERVERS_KEY) {
        None => {
            let mut servers = Table::new();
            servers.set_implicit(true);
            servers.insert(
                SERVER_NAME,
                Item::Table(build_entry_table(relay_str, server_env)),
            );
            doc.insert(SERVERS_KEY, Item::Table(servers));
            ChangeKind::Added
        }
        Some(servers_item) => {
            if let Some(tbl) = servers_item.as_table_mut() {
                if let Some(existing_entry) = tbl.get_mut(SERVER_NAME) {
                    if item_matches(existing_entry, relay_str, server_env) {
                        return Ok((ChangeKind::Unchanged, None));
                    }
                    if let Some(entry_tbl) = existing_entry.as_table_mut() {
                        update_entry_table(entry_tbl, relay_str, server_env);
                    } else if let Some(entry_inline) = existing_entry.as_inline_table_mut() {
                        update_entry_inline_table(entry_inline, relay_str, server_env);
                    } else {
                        *existing_entry = Item::Table(build_entry_table(relay_str, server_env));
                    }
                    ChangeKind::Updated
                } else {
                    tbl.insert(
                        SERVER_NAME,
                        Item::Table(build_entry_table(relay_str, server_env)),
                    );
                    ChangeKind::Added
                }
            } else if let Some(inline) = servers_item.as_inline_table_mut() {
                if let Some(existing_val) = inline.get_mut(SERVER_NAME) {
                    if existing_val
                        .as_inline_table()
                        .is_some_and(|t| inline_table_matches(t, relay_str, server_env))
                    {
                        return Ok((ChangeKind::Unchanged, None));
                    }
                    if let Some(entry_inline) = existing_val.as_inline_table_mut() {
                        update_entry_inline_table(entry_inline, relay_str, server_env);
                    } else {
                        *existing_val =
                            Value::InlineTable(build_entry_inline_table(relay_str, server_env));
                    }
                    ChangeKind::Updated
                } else {
                    inline.insert(
                        SERVER_NAME,
                        Value::InlineTable(build_entry_inline_table(relay_str, server_env)),
                    );
                    ChangeKind::Added
                }
            } else {
                return Err(format!("'{SERVERS_KEY}' must be a TOML table"));
            }
        }
    };

    Ok((change, Some(doc.to_string())))
}

/// Removes `[mcp_servers.betterrecall]` from a TOML document, leaving an empty `[mcp_servers]`
/// table if it was the last entry.
pub(crate) fn remove_toml(text: &str) -> Result<(ChangeKind, Option<String>), String> {
    let mut doc = text.parse::<DocumentMut>().map_err(|e| e.to_string())?;
    let Some(servers_item) = doc.get_mut(SERVERS_KEY) else {
        return Ok((ChangeKind::NotPresent, None));
    };

    if let Some(tbl) = servers_item.as_table_mut() {
        if tbl.remove(SERVER_NAME).is_none() {
            return Ok((ChangeKind::NotPresent, None));
        }
        if tbl.is_empty() && tbl.is_implicit() {
            tbl.set_implicit(false);
        }
        Ok((ChangeKind::Removed, Some(doc.to_string())))
    } else if let Some(inline) = servers_item.as_inline_table_mut() {
        if inline.remove(SERVER_NAME).is_none() {
            return Ok((ChangeKind::NotPresent, None));
        }
        Ok((ChangeKind::Removed, Some(doc.to_string())))
    } else {
        Err(format!("'{SERVERS_KEY}' must be a TOML table"))
    }
}

/// Renders a brand-new TOML snippet / config with `[mcp_servers.betterrecall]`.
pub(crate) fn render_new_toml(relay_str: &str, server_env: Option<(&str, &str)>) -> String {
    let mut doc = DocumentMut::new();
    let mut servers = Table::new();
    servers.set_implicit(true);
    servers.insert(
        SERVER_NAME,
        Item::Table(build_entry_table(relay_str, server_env)),
    );
    doc.insert(SERVERS_KEY, Item::Table(servers));
    doc.to_string()
}
