// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Lossless TOML editing via `toml_edit` for Codex CLI's `config.toml`.

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use crate::{ChangeKind, EntryState, SERVER_NAME};

const SERVERS_KEY: &str = "mcp_servers";

fn table_matches(tbl: &Table, relay_str: &str) -> bool {
    if tbl.len() != 2 {
        return false;
    }
    let cmd_ok = tbl.get("command").and_then(Item::as_str) == Some(relay_str);
    let args_ok = tbl
        .get("args")
        .and_then(Item::as_array)
        .is_some_and(Array::is_empty);
    cmd_ok && args_ok
}

fn inline_table_matches(tbl: &InlineTable, relay_str: &str) -> bool {
    if tbl.len() != 2 {
        return false;
    }
    let cmd_ok = tbl.get("command").and_then(Value::as_str) == Some(relay_str);
    let args_ok = tbl
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(Array::is_empty);
    cmd_ok && args_ok
}

fn item_matches(item: &Item, relay_str: &str) -> bool {
    if let Some(tbl) = item.as_table() {
        table_matches(tbl, relay_str)
    } else if let Some(inline) = item.as_inline_table() {
        inline_table_matches(inline, relay_str)
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

fn build_entry_table(relay_str: &str) -> Table {
    let mut entry = Table::new();
    entry.insert("command", toml_edit::value(relay_str));
    entry.insert("args", toml_edit::value(Array::new()));
    entry
}

fn build_entry_inline_table(relay_str: &str) -> InlineTable {
    let mut entry = InlineTable::new();
    entry.insert("command", Value::from(relay_str));
    entry.insert("args", Value::Array(Array::new()));
    entry
}

/// Inspects an existing TOML document for `[mcp_servers.betterrecall]`.
pub(crate) fn inspect_toml(text: &str, relay_str: &str) -> Result<EntryState, String> {
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
            matches: item_matches(entry, relay_str),
        })
    } else if let Some(inline) = servers_item.as_inline_table() {
        let Some(entry) = inline.get(SERVER_NAME) else {
            return Ok(EntryState::Absent);
        };
        let matches = entry
            .as_inline_table()
            .is_some_and(|t| inline_table_matches(t, relay_str));
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
) -> Result<(ChangeKind, Option<String>), String> {
    let Some(text) = existing else {
        return Ok((ChangeKind::Added, Some(render_new_toml(relay_str))));
    };

    let mut doc = text.parse::<DocumentMut>().map_err(|e| e.to_string())?;
    let change = match doc.get_mut(SERVERS_KEY) {
        None => {
            let mut servers = Table::new();
            servers.set_implicit(true);
            servers.insert(SERVER_NAME, Item::Table(build_entry_table(relay_str)));
            doc.insert(SERVERS_KEY, Item::Table(servers));
            ChangeKind::Added
        }
        Some(servers_item) => {
            if let Some(tbl) = servers_item.as_table_mut() {
                if let Some(existing_entry) = tbl.get(SERVER_NAME) {
                    if item_matches(existing_entry, relay_str) {
                        return Ok((ChangeKind::Unchanged, None));
                    }
                    tbl.insert(SERVER_NAME, Item::Table(build_entry_table(relay_str)));
                    ChangeKind::Updated
                } else {
                    tbl.insert(SERVER_NAME, Item::Table(build_entry_table(relay_str)));
                    ChangeKind::Added
                }
            } else if let Some(inline) = servers_item.as_inline_table_mut() {
                if let Some(existing_val) = inline.get(SERVER_NAME) {
                    if existing_val
                        .as_inline_table()
                        .is_some_and(|t| inline_table_matches(t, relay_str))
                    {
                        return Ok((ChangeKind::Unchanged, None));
                    }
                    inline.insert(
                        SERVER_NAME,
                        Value::InlineTable(build_entry_inline_table(relay_str)),
                    );
                    ChangeKind::Updated
                } else {
                    inline.insert(
                        SERVER_NAME,
                        Value::InlineTable(build_entry_inline_table(relay_str)),
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
pub(crate) fn render_new_toml(relay_str: &str) -> String {
    let mut doc = DocumentMut::new();
    let mut servers = Table::new();
    servers.set_implicit(true);
    servers.insert(SERVER_NAME, Item::Table(build_entry_table(relay_str)));
    doc.insert(SERVERS_KEY, Item::Table(servers));
    doc.to_string()
}
