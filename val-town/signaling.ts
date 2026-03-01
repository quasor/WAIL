// WAIL HTTP Signaling Server for Val Town
// Deploy: paste into a Val Town HTTP val at https://wail.val.run/
//
// Replaces the WebSocket signaling server with HTTP polling.
// Signaling is only used during connection setup (~12-22 messages).
// Once WebRTC DataChannels open, this server is never contacted again.

import { sqlite } from "https://esm.town/v/std/sqlite";

// ---------------------------------------------------------------------------
// Schema (idempotent)
// ---------------------------------------------------------------------------

await sqlite.batch([
  `CREATE TABLE IF NOT EXISTS peers (
    room TEXT NOT NULL,
    peer_id TEXT NOT NULL,
    last_seen INTEGER NOT NULL,
    PRIMARY KEY (room, peer_id)
  )`,
  `CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    room TEXT NOT NULL,
    to_peer TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
  )`,
  `CREATE INDEX IF NOT EXISTS idx_messages_dest
    ON messages (room, to_peer, id)`,
  `CREATE TABLE IF NOT EXISTS rooms (
    room TEXT PRIMARY KEY,
    password_hash TEXT NOT NULL
  )`,
]);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function now(): number {
  return Math.floor(Date.now() / 1000);
}

async function hashPassword(password: string): Promise<string> {
  const data = new TextEncoder().encode(password);
  const hash = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(hash))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

function json(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: {
      "Content-Type": "application/json",
      "Access-Control-Allow-Origin": "*",
    },
  });
}

async function enqueueForRoom(
  room: string,
  excludePeer: string,
  body: unknown,
): Promise<void> {
  const rows = await sqlite.execute({
    sql: "SELECT peer_id FROM peers WHERE room = ? AND peer_id != ?",
    args: [room, excludePeer],
  });
  const ts = now();
  const bodyStr = JSON.stringify(body);
  for (const row of rows.rows) {
    await sqlite.execute({
      sql: "INSERT INTO messages (room, to_peer, body, created_at) VALUES (?, ?, ?, ?)",
      args: [room, row[0] as string, bodyStr, ts],
    });
  }
}

async function cleanStalePeers(room: string): Promise<void> {
  const cutoff = now() - 30;
  const stale = await sqlite.execute({
    sql: "SELECT peer_id FROM peers WHERE room = ? AND last_seen < ?",
    args: [room, cutoff],
  });
  for (const row of stale.rows) {
    const stalePeer = row[0] as string;
    await sqlite.execute({
      sql: "DELETE FROM peers WHERE room = ? AND peer_id = ?",
      args: [room, stalePeer],
    });
    // Notify remaining peers
    await enqueueForRoom(room, stalePeer, {
      type: "PeerLeft",
      peer_id: stalePeer,
    });
  }
  // If no peers remain in the room, delete the room password so it can be re-created
  const remaining = await sqlite.execute({
    sql: "SELECT COUNT(*) FROM peers WHERE room = ?",
    args: [room],
  });
  if ((remaining.rows[0][0] as number) === 0) {
    await sqlite.execute({
      sql: "DELETE FROM rooms WHERE room = ?",
      args: [room],
    });
  }

  // Clean old messages (>60s)
  const msgCutoff = now() - 60;
  await sqlite.execute({
    sql: "DELETE FROM messages WHERE created_at < ?",
    args: [msgCutoff],
  });
}

// ---------------------------------------------------------------------------
// Request handler
// ---------------------------------------------------------------------------

export default async function(req: Request): Promise<Response> {
  // CORS preflight
  if (req.method === "OPTIONS") {
    return new Response(null, {
      headers: {
        "Access-Control-Allow-Origin": "*",
        "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
        "Access-Control-Allow-Headers": "Content-Type",
      },
    });
  }

  const url = new URL(req.url);
  const action = url.searchParams.get("action");

  try {
    switch (action) {
      // -------------------------------------------------------------------
      // POST ?action=join  body: { room, peer_id, password }
      // -------------------------------------------------------------------
      case "join": {
        const { room, peer_id, password } = await req.json();
        if (!room || !peer_id) return json({ error: "room and peer_id required" }, 400);
        if (!password) return json({ error: "password required" }, 400);

        // Check or create room password
        const pwHash = await hashPassword(password);
        const roomRow = await sqlite.execute({
          sql: "SELECT password_hash FROM rooms WHERE room = ?",
          args: [room],
        });
        if (roomRow.rows.length === 0) {
          // First peer creates the room
          await sqlite.execute({
            sql: "INSERT INTO rooms (room, password_hash) VALUES (?, ?)",
            args: [room, pwHash],
          });
        } else {
          const storedHash = roomRow.rows[0][0] as string;
          if (storedHash !== pwHash) {
            return json({ error: "invalid password" }, 401);
          }
        }

        const ts = now();

        // Upsert peer
        await sqlite.execute({
          sql: `INSERT INTO peers (room, peer_id, last_seen) VALUES (?, ?, ?)
                ON CONFLICT (room, peer_id) DO UPDATE SET last_seen = ?`,
          args: [room, peer_id, ts, ts],
        });

        // Get current peers (excluding self)
        const rows = await sqlite.execute({
          sql: "SELECT peer_id FROM peers WHERE room = ? AND peer_id != ?",
          args: [room, peer_id],
        });
        const peers = rows.rows.map((r) => r[0] as string);

        // Enqueue PeerJoined for existing peers
        await enqueueForRoom(room, peer_id, {
          type: "PeerJoined",
          peer_id,
        });

        return json({ peers });
      }

      // -------------------------------------------------------------------
      // POST ?action=leave  body: { room, peer_id }
      // -------------------------------------------------------------------
      case "leave": {
        const { room, peer_id } = await req.json();
        if (!room || !peer_id) return json({ error: "room and peer_id required" }, 400);

        await sqlite.execute({
          sql: "DELETE FROM peers WHERE room = ? AND peer_id = ?",
          args: [room, peer_id],
        });

        // Enqueue PeerLeft for remaining peers
        await enqueueForRoom(room, peer_id, {
          type: "PeerLeft",
          peer_id,
        });

        // Clean up messages for this peer
        await sqlite.execute({
          sql: "DELETE FROM messages WHERE room = ? AND to_peer = ?",
          args: [room, peer_id],
        });

        return json({ ok: true });
      }

      // -------------------------------------------------------------------
      // POST ?action=signal  body: SignalMessage (has to, from, payload)
      // -------------------------------------------------------------------
      case "signal": {
        const body = await req.json();
        if (!body.to || !body.from) return json({ error: "to and from required" }, 400);

        // Find the room for the sender
        const row = await sqlite.execute({
          sql: "SELECT room FROM peers WHERE peer_id = ? LIMIT 1",
          args: [body.from],
        });
        if (row.rows.length === 0) return json({ error: "sender not in any room" }, 404);

        const room = row.rows[0][0] as string;
        const ts = now();

        await sqlite.execute({
          sql: "INSERT INTO messages (room, to_peer, body, created_at) VALUES (?, ?, ?, ?)",
          args: [room, body.to, JSON.stringify(body), ts],
        });

        return json({ ok: true });
      }

      // -------------------------------------------------------------------
      // GET ?action=poll&room=X&peer_id=Y&after=SEQ
      // -------------------------------------------------------------------
      case "poll": {
        const room = url.searchParams.get("room");
        const peer_id = url.searchParams.get("peer_id");
        const after = parseInt(url.searchParams.get("after") || "0", 10);

        if (!room || !peer_id) return json({ error: "room and peer_id required" }, 400);

        // Heartbeat: update last_seen
        await sqlite.execute({
          sql: "UPDATE peers SET last_seen = ? WHERE room = ? AND peer_id = ?",
          args: [now(), room, peer_id],
        });

        // Clean stale peers
        await cleanStalePeers(room);

        // Fetch queued messages
        const rows = await sqlite.execute({
          sql: "SELECT id, body FROM messages WHERE room = ? AND to_peer = ? AND id > ? ORDER BY id ASC LIMIT 50",
          args: [room, peer_id, after],
        });

        const messages = rows.rows.map((r) => ({
          seq: r[0] as number,
          body: JSON.parse(r[1] as string),
        }));

        return json({ messages });
      }

      default:
        return json({ error: `unknown action: ${action}` }, 400);
    }
  } catch (e: any) {
    return json({ error: e.message }, 500);
  }
}
