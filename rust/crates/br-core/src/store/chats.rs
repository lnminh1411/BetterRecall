// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Saved cloud chats (`ai.chatHistory`) and the local daily AI budget counter in `store.db`.

use br_contracts::{ChatMessage, ChatRecord, ChatSource, ChatSummary};
use chrono::{Duration as ChronoDuration, Utc};

use crate::core_json::CoreJson;
use crate::sqlite::{Connection, Value};
use crate::store::StoreError;
use crate::store::encrypted_store::{EncryptedStore, checkpoint_locked, in_list, ms_to_utc};

impl EncryptedStore {
    pub const CHAT_TITLE_MAX: usize = 120;

    pub(crate) fn migrate_chats(w: &Connection) -> Result<(), StoreError> {
        w.exec_batch(
            "CREATE TABLE IF NOT EXISTS chats(id TEXT PRIMARY KEY, title TEXT NOT NULL, created INTEGER NOT NULL, updated INTEGER NOT NULL,
                 provider TEXT NOT NULL, model TEXT NOT NULL, transcript TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS ix_chats_updated ON chats(updated);
             CREATE TABLE IF NOT EXISTS chat_messages(chat_id TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, role TEXT NOT NULL,
                 text TEXT NOT NULL, sources TEXT NOT NULL, PRIMARY KEY(chat_id, seq));
             CREATE INDEX IF NOT EXISTS ix_chat_messages_ts ON chat_messages(ts);
             CREATE TABLE IF NOT EXISTS ai_usage(day TEXT PRIMARY KEY, requests INTEGER NOT NULL, tokens INTEGER NOT NULL);",
        )?;
        let mut has_endpoint = false;
        {
            let mut stmt = w.prepare("SELECT name FROM pragma_table_info('chats')")?;
            while stmt.step()? {
                if stmt.get_string(0)? == "endpoint" {
                    has_endpoint = true;
                }
            }
        }
        if !has_endpoint {
            w.exec_batch("ALTER TABLE chats ADD COLUMN endpoint TEXT NOT NULL DEFAULT ''")?;
        }
        if w.query_scalar_string("SELECT v FROM meta WHERE k='chat_cite_sweep'", &[])?
            .is_none()
        {
            w.exec_batch(
                "DELETE FROM chats WHERE id IN (
                     SELECT DISTINCT m.chat_id FROM chat_messages m, json_each(m.sources) j
                     WHERE json_extract(j.value, '$.id') NOT IN (SELECT id FROM captures))",
            )?;
            w.exec_batch("DELETE FROM chat_messages WHERE chat_id NOT IN (SELECT id FROM chats)")?;
            w.exec_batch("INSERT OR REPLACE INTO meta(k, v) VALUES('chat_cite_sweep', '1')")?;
        }
        Ok(())
    }

    pub fn chats(&self, limit: i32) -> Result<Vec<ChatSummary>, StoreError> {
        let limit = limit.clamp(1, 1000);
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let mut stmt = r.prepare(
            "SELECT c.id, c.title, c.created, c.updated, (SELECT COUNT(*) FROM chat_messages m WHERE m.chat_id = c.id)
             FROM chats c ORDER BY c.updated DESC LIMIT $n",
        )?;
        stmt.bind_named("$n", Value::Int(limit))?;
        let mut list = Vec::new();
        while stmt.step()? {
            list.push(ChatSummary {
                id: stmt.get_string(0)?,
                title: stmt.get_string(1)?,
                created: ms_to_utc(stmt.get_i64(2)?),
                updated: ms_to_utc(stmt.get_i64(3)?),
                messages: stmt.get_i32(4)?,
            });
        }
        Ok(list)
    }

    pub fn get_chat(&self, id: &str) -> Result<Option<ChatRecord>, StoreError> {
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let mut head = {
            let mut stmt = r.prepare(
                "SELECT title, created, updated, provider, model, transcript, endpoint FROM chats WHERE id=$id",
            )?;
            stmt.bind_named("$id", Value::Text(id))?;
            if !stmt.step()? {
                return Ok(None);
            }
            ChatRecord {
                id: id.to_string(),
                title: stmt.get_string(0)?,
                created: ms_to_utc(stmt.get_i64(1)?),
                updated: ms_to_utc(stmt.get_i64(2)?),
                provider: stmt.get_string(3)?,
                model: stmt.get_string(4)?,
                transcript: stmt.get_string(5)?,
                messages: Vec::new(),
                endpoint: stmt.get_string(6)?,
            }
        };
        let mut msgs = Vec::new();
        {
            let mut stmt = r.prepare(
                "SELECT seq, ts, role, text, sources FROM chat_messages WHERE chat_id=$id ORDER BY seq",
            )?;
            stmt.bind_named("$id", Value::Text(id))?;
            while stmt.step()? {
                let sources_json = stmt.get_string(4)?;
                msgs.push(ChatMessage {
                    seq: stmt.get_i32(0)?,
                    ts: ms_to_utc(stmt.get_i64(1)?),
                    role: stmt.get_string(2)?,
                    text: stmt.get_string(3)?,
                    sources: Some(parse_sources(&sources_json)),
                });
            }
        }
        head.messages = msgs;
        Ok(Some(head))
    }

    pub fn save_chat(&self, chat: &ChatRecord) -> Result<(), StoreError> {
        let st = self.wl.lock();
        let w = st.w()?;
        let tx = Self::begin_write(w)?;
        let title = Self::cut(chat.title.trim(), Self::CHAT_TITLE_MAX);
        w.execute(
            "INSERT INTO chats(id, title, created, updated, provider, model, transcript, endpoint) VALUES($id, $t, $c, $u, $p, $m, $x, $e)
             ON CONFLICT(id) DO UPDATE SET title=excluded.title, updated=excluded.updated, provider=excluded.provider,
                 model=excluded.model, transcript=excluded.transcript, endpoint=excluded.endpoint",
            &[
                ("$id", Value::Text(&chat.id)),
                ("$t", Value::Text(&title)),
                ("$c", Value::Int64(chat.created.timestamp_millis())),
                ("$u", Value::Int64(chat.updated.timestamp_millis())),
                ("$p", Value::Text(&chat.provider)),
                ("$m", Value::Text(&chat.model)),
                ("$x", Value::Text(&chat.transcript)),
                ("$e", Value::Text(&chat.endpoint)),
            ],
        )?;
        let max_seq = w
            .query_scalar_i64(
                "SELECT COALESCE(MAX(seq), -1) FROM chat_messages WHERE chat_id=$id",
                &[("$id", Value::Text(&chat.id))],
            )?
            .unwrap_or(-1);
        for m in chat.messages.iter().filter(|m| i64::from(m.seq) > max_seq) {
            let empty: Vec<ChatSource> = Vec::new();
            let sources_slice = m.sources.as_deref().unwrap_or(&empty);
            let src_json = CoreJson::to_string(&sources_slice)?;
            w.execute(
                "INSERT INTO chat_messages(chat_id, seq, ts, role, text, sources) VALUES($id, $s, $ts, $r, $x, $src)",
                &[
                    ("$id", Value::Text(&chat.id)),
                    ("$s", Value::Int(m.seq)),
                    ("$ts", Value::Int64(m.ts.timestamp_millis())),
                    ("$r", Value::Text(&m.role)),
                    ("$x", Value::Text(&m.text)),
                    ("$src", Value::Text(&src_json)),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn rename_chat(&self, id: &str, title: &str) -> Result<bool, StoreError> {
        let title = title.trim();
        if title.is_empty() {
            return Ok(false);
        }
        let st = self.wl.lock();
        let cut_title = Self::cut(title, Self::CHAT_TITLE_MAX);
        let n = st.w()?.execute(
            "UPDATE chats SET title=$t WHERE id=$id",
            &[("$t", Value::Text(&cut_title)), ("$id", Value::Text(id))],
        )?;
        Ok(n > 0)
    }

    pub fn delete_chats(&self, id: Option<&str>) -> Result<usize, StoreError> {
        let st = self.wl.lock();
        let w = st.w()?;
        let n = match id {
            None => {
                w.execute("DELETE FROM chat_messages", &[])?;
                w.execute("DELETE FROM chats", &[])?
            }
            Some(chat_id) => {
                w.execute(
                    "DELETE FROM chat_messages WHERE chat_id=$id",
                    &[("$id", Value::Text(chat_id))],
                )?;
                w.execute(
                    "DELETE FROM chats WHERE id=$id",
                    &[("$id", Value::Text(chat_id))],
                )?
            }
        };
        if n > 0 {
            checkpoint_locked(&st);
        }
        Ok(n)
    }

    pub(crate) fn delete_chats_older_locked(w: &Connection, cutoff_ms: i64) -> Result<(), StoreError> {
        w.execute(
            "DELETE FROM chats WHERE created < $c",
            &[("$c", Value::Int64(cutoff_ms))],
        )?;
        w.exec_batch("DELETE FROM chat_messages WHERE chat_id NOT IN (SELECT id FROM chats)")?;
        Ok(())
    }

    pub(crate) fn delete_chats_citing_locked(w: &Connection, rids: &[i64]) -> Result<(), StoreError> {
        for chunk in rids.chunks(500) {
            let list = in_list(chunk);
            w.exec_batch(&format!(
                "DELETE FROM chats WHERE id IN (
                     SELECT DISTINCT m.chat_id FROM chat_messages m, json_each(m.sources) j
                     WHERE json_extract(j.value, '$.id') IN (SELECT id FROM captures WHERE rid IN ({list})))"
            ))?;
        }
        w.exec_batch("DELETE FROM chat_messages WHERE chat_id NOT IN (SELECT id FROM chats)")?;
        Ok(())
    }

    pub(crate) fn delete_chats_in_window_locked(
        w: &Connection,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<(), StoreError> {
        w.execute(
            "DELETE FROM chats WHERE id IN (SELECT DISTINCT chat_id FROM chat_messages WHERE ts >= $from AND ts <= $to)",
            &[("$from", Value::Int64(from_ms)), ("$to", Value::Int64(to_ms))],
        )?;
        w.exec_batch("DELETE FROM chat_messages WHERE chat_id NOT IN (SELECT id FROM chats)")?;
        Ok(())
    }

    pub fn ai_usage(&self, day: &str) -> Result<(i32, i64), StoreError> {
        let rl = self.rl.lock().expect("rl");
        let r = rl.as_ref().ok_or(StoreError::Disposed)?;
        let mut stmt = r.prepare("SELECT requests, tokens FROM ai_usage WHERE day=$d")?;
        stmt.bind_named("$d", Value::Text(day))?;
        if stmt.step()? {
            Ok((stmt.get_i32(0)?, stmt.get_i64(1)?))
        } else {
            Ok((0, 0))
        }
    }

    pub fn add_ai_usage(&self, day: &str, requests: i32, tokens: i64) -> Result<(), StoreError> {
        let st = self.wl.lock();
        let w = st.w()?;
        w.execute(
            "INSERT INTO ai_usage(day, requests, tokens) VALUES($d, $r, $t)
             ON CONFLICT(day) DO UPDATE SET requests = requests + excluded.requests, tokens = tokens + excluded.tokens",
            &[
                ("$d", Value::Text(day)),
                ("$r", Value::Int(requests.max(0))),
                ("$t", Value::Int64(tokens.max(0))),
            ],
        )?;
        let old_day = (Utc::now() - ChronoDuration::days(60))
            .format("%Y-%m-%d")
            .to_string();
        w.execute(
            "DELETE FROM ai_usage WHERE day < $old",
            &[("$old", Value::Text(&old_day))],
        )?;
        Ok(())
    }
}

fn parse_sources(json: &str) -> Vec<ChatSource> {
    serde_json::from_str(json).unwrap_or_default()
}
