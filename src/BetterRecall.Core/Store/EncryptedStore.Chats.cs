// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Store;

/// <summary>
/// Saved cloud chats (ai.chatHistory) and the local daily AI budget counter, in store.db so they are encrypted with
/// the store. A time-window purge deletes chats with a message in the window; purge all deletes the store.
/// </summary>
public sealed partial class EncryptedStore
{
    public const int ChatTitleMax = 120;

    /// <summary>0.2 batch 4: chats, chat_messages, ai_usage. Idempotent (CREATE IF NOT EXISTS), schema number unchanged.</summary>
    private void MigrateChats() => Exec(_w, """
        CREATE TABLE IF NOT EXISTS chats(id TEXT PRIMARY KEY, title TEXT NOT NULL, created INTEGER NOT NULL, updated INTEGER NOT NULL,
            provider TEXT NOT NULL, model TEXT NOT NULL, transcript TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS ix_chats_updated ON chats(updated);
        CREATE TABLE IF NOT EXISTS chat_messages(chat_id TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, role TEXT NOT NULL,
            text TEXT NOT NULL, sources TEXT NOT NULL, PRIMARY KEY(chat_id, seq));
        CREATE INDEX IF NOT EXISTS ix_chat_messages_ts ON chat_messages(ts);
        CREATE TABLE IF NOT EXISTS ai_usage(day TEXT PRIMARY KEY, requests INTEGER NOT NULL, tokens INTEGER NOT NULL);
        """);

    public IReadOnlyList<ChatSummary> Chats(int limit)
    {
        limit = Math.Clamp(limit, 1, 1000);
        lock (_rl)
        {
            ThrowIfDisposed();
            using var cmd = Cmd(_r, """
                SELECT c.id, c.title, c.created, c.updated, (SELECT COUNT(*) FROM chat_messages m WHERE m.chat_id = c.id)
                FROM chats c ORDER BY c.updated DESC LIMIT $n
                """, ("$n", limit));
            using var rd = cmd.ExecuteReader();
            var list = new List<ChatSummary>();
            while (rd.Read())
                list.Add(new ChatSummary(rd.GetString(0), rd.GetString(1), At(rd.GetInt64(2)), At(rd.GetInt64(3)), rd.GetInt32(4)));
            return list;
        }
    }

    public ChatRecord? GetChat(string id)
    {
        lock (_rl)
        {
            ThrowIfDisposed();
            ChatRecord head;
            using (var cmd = Cmd(_r, "SELECT title, created, updated, provider, model, transcript FROM chats WHERE id=$id", ("$id", id)))
            using (var rd = cmd.ExecuteReader())
            {
                if (!rd.Read()) return null;
                head = new ChatRecord(id, rd.GetString(0), At(rd.GetInt64(1)), At(rd.GetInt64(2)), rd.GetString(3), rd.GetString(4), rd.GetString(5), []);
            }
            var msgs = new List<ChatMessage>();
            using (var cmd = Cmd(_r, "SELECT seq, ts, role, text, sources FROM chat_messages WHERE chat_id=$id ORDER BY seq", ("$id", id)))
            using (var rd = cmd.ExecuteReader())
                while (rd.Read())
                    msgs.Add(new ChatMessage(rd.GetInt32(0), At(rd.GetInt64(1)), rd.GetString(2), rd.GetString(3), Sources(rd.GetString(4))));
            return head with { Messages = msgs };
        }
    }

    public void SaveChat(ChatRecord chat)
    {
        lock (_wl)
        {
            ThrowIfDisposed();
            using var tx = _w.BeginTransaction();
            Exec(_w, """
                INSERT INTO chats(id, title, created, updated, provider, model, transcript) VALUES($id, $t, $c, $u, $p, $m, $x)
                ON CONFLICT(id) DO UPDATE SET title=excluded.title, updated=excluded.updated, provider=excluded.provider,
                    model=excluded.model, transcript=excluded.transcript
                """,
                ("$id", chat.Id), ("$t", Cut(chat.Title.Trim(), ChatTitleMax)), ("$c", chat.Created.ToUnixTimeMilliseconds()),
                ("$u", chat.Updated.ToUnixTimeMilliseconds()), ("$p", chat.Provider), ("$m", chat.Model), ("$x", chat.Transcript));
            long maxSeq;
            using (var cmd = Cmd(_w, "SELECT COALESCE(MAX(seq), -1) FROM chat_messages WHERE chat_id=$id", ("$id", chat.Id)))
                maxSeq = Convert.ToInt64(cmd.ExecuteScalar(), CultureInfo.InvariantCulture);
            foreach (var m in chat.Messages.Where(m => m.Seq > maxSeq))
                Exec(_w, "INSERT INTO chat_messages(chat_id, seq, ts, role, text, sources) VALUES($id, $s, $ts, $r, $x, $src)",
                    ("$id", chat.Id), ("$s", m.Seq), ("$ts", m.Ts.ToUnixTimeMilliseconds()), ("$r", m.Role), ("$x", m.Text),
                    ("$src", JsonSerializer.Serialize(m.Sources?.ToList() ?? [], CoreJson.Default.ListChatSource)));
            tx.Commit();
        }
    }

    public bool RenameChat(string id, string title)
    {
        title = (title ?? "").Trim();
        if (title.Length == 0) return false;
        lock (_wl)
        {
            ThrowIfDisposed();
            using var cmd = Cmd(_w, "UPDATE chats SET title=$t WHERE id=$id", ("$t", Cut(title, ChatTitleMax)), ("$id", id));
            return cmd.ExecuteNonQuery() > 0;
        }
    }

    public int DeleteChats(string? id)
    {
        lock (_wl)
        {
            ThrowIfDisposed();
            int n;
            if (id == null)
            {
                Exec(_w, "DELETE FROM chat_messages");
                using var cmd = Cmd(_w, "DELETE FROM chats");
                n = cmd.ExecuteNonQuery();
            }
            else
            {
                Exec(_w, "DELETE FROM chat_messages WHERE chat_id=$id", ("$id", id));
                using var cmd = Cmd(_w, "DELETE FROM chats WHERE id=$id", ("$id", id));
                n = cmd.ExecuteNonQuery();
            }
            if (n > 0) CheckpointLocked();
            return n;
        }
    }

    /// <summary>Caller holds _wl. Chats with any message in [from, to].</summary>
    private void DeleteChatsInWindowLocked(DateTimeOffset from, DateTimeOffset to)
    {
        var p = new (string, object?)[] { ("$from", from.ToUnixTimeMilliseconds()), ("$to", to.ToUnixTimeMilliseconds()) };
        const string ids = "SELECT DISTINCT chat_id FROM chat_messages WHERE ts >= $from AND ts <= $to";
        Exec(_w, $"DELETE FROM chats WHERE id IN ({ids})", p);
        Exec(_w, "DELETE FROM chat_messages WHERE chat_id NOT IN (SELECT id FROM chats)");
    }

    public (int Requests, long Tokens) AiUsage(string day)
    {
        lock (_rl)
        {
            ThrowIfDisposed();
            using var cmd = Cmd(_r, "SELECT requests, tokens FROM ai_usage WHERE day=$d", ("$d", day));
            using var rd = cmd.ExecuteReader();
            return rd.Read() ? (rd.GetInt32(0), rd.GetInt64(1)) : (0, 0);
        }
    }

    public void AddAiUsage(string day, int requests, long tokens)
    {
        lock (_wl)
        {
            ThrowIfDisposed();
            Exec(_w, """
                INSERT INTO ai_usage(day, requests, tokens) VALUES($d, $r, $t)
                ON CONFLICT(day) DO UPDATE SET requests = requests + excluded.requests, tokens = tokens + excluded.tokens
                """, ("$d", day), ("$r", Math.Max(0, requests)), ("$t", Math.Max(0, tokens)));
            Exec(_w, "DELETE FROM ai_usage WHERE day < $old", ("$old", DateTime.UtcNow.AddDays(-60).ToString("yyyy-MM-dd", CultureInfo.InvariantCulture)));
        }
    }


    private static DateTimeOffset At(long ms) => DateTimeOffset.FromUnixTimeMilliseconds(ms);

    private static List<ChatSource> Sources(string json)
    {
        try { return JsonSerializer.Deserialize(json, CoreJson.Default.ListChatSource) ?? []; }
        catch (JsonException) { return []; }
    }
}
