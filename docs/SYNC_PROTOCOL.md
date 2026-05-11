# RetinaTag LAN sync — wire protocol (Phase-1)

This document is the source of truth for the cross-device sync protocol.
Phase-1 ships with v1.5.76 on Windows. The Mac port (separate codebase
under `MACOS/`) needs to implement this same wire format for the two
to discover and pair with each other.

## Discovery — mDNS / Bonjour

Both sides advertise the service:

```
_retinatag._tcp.local.
```

TXT record properties (string keys → string values):

| Key           | Value                                              |
|---------------|----------------------------------------------------|
| `device_id`   | `rt-<16 hex chars>` — stable, per-install          |
| `device_name` | Human-readable, user-editable                      |
| `pubkey`      | Ed25519 public key, **32 bytes, base64url no pad** |
| `v`           | Protocol version, currently `"1"`                  |

Port is whatever TCP port the device's HTTP server is listening on
(chosen at runtime from the ephemeral range). Both v4 + v6 addresses
are valid.

## Identity

Each install holds an Ed25519 keypair, generated once on first
`sync_enable`:

- Stored in SQLite (`sync_identity` table) — secret key NEVER leaves
  the host. Only the public key is advertised.
- `device_id = "rt-" + lowercase_hex(random_u64)`
- `device_name` defaults to OS hostname; user can rename anytime.

## HTTP transport — JSON over plain HTTP

All endpoints are JSON over HTTP (no TLS). Authentication is per-message
via Ed25519 signatures in Phase-2; Phase-1 ships unauthenticated `/ping`
+ a one-shot pair-code-guarded `/pair`.

### `GET /ping` — identity probe (anyone can call)

Response:
```json
{
  "device_id": "rt-3f1a92c40b8d7e22",
  "device_name": "Buğra'nın Mac'i",
  "public_key_b64": "qX7…",
  "protocol_version": 1,
  "app": "RetinaTag"
}
```

`app` is the literal string `"RetinaTag"`. If anything else is returned,
the caller MUST refuse to proceed — defensive guard against an unrelated
service squatting on the same Bonjour domain.

### `POST /pair` — exchange identities under a pair code

Request body:
```json
{
  "from": {
    "device_id":       "rt-…",
    "device_name":     "…",
    "public_key_b64":  "…"
  },
  "code": "123456"
}
```

The responder:
1. Looks up its current outstanding pair code (held in memory only,
   minted by the user via `sync_mint_pair_code`, TTL = 5 minutes).
2. Compares constant-time against `code`. Mismatch → HTTP 401.
3. No outstanding code or expired → HTTP 403.
4. On success, inserts the requester into `sync_peers` and burns the
   code (single-use).

Response:
```json
{
  "identity": {
    "device_id":      "rt-…",
    "device_name":    "…",
    "public_key_b64": "…"
  },
  "paired_at": 1715367890
}
```

The requester persists this identity in its own `sync_peers`.

## Phase-2 (sync verbs — not yet implemented)

The Phase-1 transport already accepts arbitrary signed envelopes; the
data-sync verbs land in the v1.5.77 cycle. Sketch for the Mac port to
prepare for:

```
POST /sync/since
  body:  { cursor: <unix epoch>, kinds: ["tag","rating","favorite","description"] }
  sig:   Ed25519 over JSON body, base64url, in header `X-RT-Sig`
  from:  device_id in header `X-RT-From`

  response:
    { rows: [
        { kind: "tag", photo_hash: "xxh3:…", tag: "beach",
          confidence: 0.91, source: "ai", updated_at: 1715369000,
          deleted: false, by_device: "rt-…" },
        ...
      ],
      next_cursor: 1715369999
    }
```

Photo identity is `photo_hash` (xxh3 prefix, already in the existing
`photos.hash` column). Conflict resolution: last-write-wins by
`updated_at`. Deletes are tombstones (`deleted: true`) so the other
side can fan out the deletion idempotently.

## Schema additions (v1.5.76)

```sql
CREATE TABLE sync_identity (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    device_id   TEXT NOT NULL,
    device_name TEXT NOT NULL,
    secret_key  BLOB NOT NULL,    -- 32 bytes
    public_key  BLOB NOT NULL,    -- 32 bytes
    enabled     INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);

CREATE TABLE sync_peers (
    device_id   TEXT PRIMARY KEY,
    device_name TEXT NOT NULL,
    public_key  BLOB NOT NULL,
    last_addr   TEXT,
    last_seen   INTEGER,
    paired_at   INTEGER NOT NULL
);
```

## Off-by-default

Until the user toggles "Enable network sync" in Settings, neither the
mDNS broadcast nor the HTTP listener runs. `sync_identity.enabled = 0`
on a fresh install. The keypair is lazily minted on first enable.
