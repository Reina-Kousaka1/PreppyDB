# HTTP API v1

Base URL: `http://<host>:<port>`. All request and response bodies are UTF-8 JSON.
The API version is part of the path (`/v1`), so incompatible changes ship as `/v2`.

## Envelope

Success:

```json
{ "ok": true, "data": { "...": "..." } }
```

Failure:

```json
{ "ok": false, "error": { "code": "conflict", "message": "version mismatch", "retryable": true } }
```

Every response carries `X-Request-Id` (also returned when logging the same id), and
`Content-Type: application/json; charset=utf-8`.

## Authentication

Three ways to authenticate, all as `Authorization: Bearer <token>`:

| Kind | Created with | Lifetime |
| --- | --- | --- |
| API token | `preppydb token create --name X --role read\|write\|admin` or `--bootstrap-token` | until revoked |
| Session token | `POST /v1/auth/login` | 8 hours (in memory, lost on restart) |
| Anonymous | — | only `/healthz`, `/readyz`, `/v1/version`, the login endpoint and the static UI |

Roles: `read` < `write` < `admin`.

### Experimental document query

When an administrator configures an absolute `ruby_planner_path`, clients with the
`read` role may call `POST /v1/collections/{collection}/query` with, for example,
`{"version":1,"filter":{"field":"name","op":"eq","value":"A"},"limit":100}`.
The optional filter supports `eq`, `gte`, and `lte` on scalar values. Missing fields
do not match, including `eq` with `null`. Range comparisons apply only to numbers or
strings of the same kind; there is no implicit string/number conversion. Limits are
1–1000. Rust validates the Ruby plan, rechecks the complete filter and revalidates
index metadata under the read lock. Range and numeric index plans are not yet enabled.
The planner runs as a separate process, not an OS sandbox. This experimental endpoint
is disabled by default and is not a stable query-language commitment.

### Controlled Lua mutation scripts

`lua_execution_enabled` defaults to `false`. When an administrator explicitly enables
it, only an `admin` principal may call `POST /v1/lua/execute`. The body contains a
`script` string and an optional JSON `input`. The script returns exactly
`{ result = <JSON>, operations = <array> }`; Rust accepts only `put` and `delete`
operations, validates collection policy and operation shapes, and sends the complete
list through the ordinary compare-and-set storage batch path. Each operation must
include `expected_version` (`0` means insert-only; another value must match the
document's current version). Scripts have no database handle
and execute before any storage lock is acquired. If execution or validation fails,
none of the returned operations are submitted.

The embedded Lua 5.4 VM has a 128 KiB source limit, 2 MiB input/output limits, a 16 MiB
VM memory limit, a 2,000,000-instruction budget and at most 1,000 mutations. It does
not load filesystem, network, package, debug, string, dynamic-load or protected-call
libraries. JSON `null` is `preppydb.null` and can be checked with
`preppydb.is_null(value)`; use `preppydb.array()` to express an empty JSON array (an
unmarked empty Lua table is an empty JSON object). Scripts are supplied per request;
there is no persistent function catalog, trigger system, read/query access from Lua,
or durable retry/idempotency mechanism. If a storage commit may have reached the WAL
but append/sync reports an error, the API returns `commit_outcome_unknown`; reopen and
inspect state before retrying the mutation.

Example:

```lua
local amount = input.amount
return {
  result = { credited = amount },
  operations = preppydb.array({
    { op = "put", collection = "wallets", key = "user-1", expected_version = 0, value = { balance = amount } }
  })
}
```

Enabling the endpoint grants configured administrators the ability to run bounded
Lua computation and request writes as the server identity. It should remain disabled
unless this capability is intentionally administered.

* `read` — read documents, scan, index queries, stats.
* `write` — everything `read` plus document writes, batches, transactions, pricing ticks,
  purchases.
* `admin` — everything, plus collection/index creation and deletion, checkpoints,
  compaction, verify, backups, personality bootstrap, audit verification.

Passwords must be at least 12 characters and are stored as PBKDF2-SHA-256 with a per-user
salt. Tokens are stored as SHA-256 digests; the plaintext is shown exactly once.

### `POST /v1/auth/login`

```json
{ "username": "admin", "password": "at-least-12-characters" }
```

```json
{ "ok": true, "data": { "token": "…", "expires_in_ms": 28800000,
  "principal": { "kind": "session", "name": "admin", "roles": ["admin"] } } }
```

The session token is stored in memory only: restarting the server ends every session
(API tokens are unaffected).

Unknown user and wrong password return the identical `401 invalid_credentials` body.

### `POST /v1/auth/logout` — invalidates the presented session token.
### `GET /v1/auth/whoami` — `{kind, name, roles}` of the current principal.

## Public endpoints

| Endpoint | Meaning |
| --- | --- |
| `GET /healthz` | process alive: `{"status":"ok","checks":{}}` |
| `GET /readyz` | ready to serve reads/writes: `{"ready":true,"read_only":false,"checks":{"storage":"ok"}}` — `503` when storage is unavailable |
| `GET /v1/version` | `{"engine":"preppydb","version":"0.1.0","api":"v1"}` |
| `GET /` | web admin UI (static) |

## Collections

### `GET /v1/collections`

```json
{ "ok": true, "data": { "collections": [
  { "name": "themes", "documents": 3, "created_at": 1793817600000,
    "has_schema": true, "indexes": ["by_name"], "max_docs": 10000 }
] } }
```

### `POST /v1/collections` (admin)

```json
{ "name": "themes",
  "schema": { "type": "object", "required": ["name"], "properties": { "name": { "type": "string" } } },
  "max_docs": 10000,
  "default_ttl_ms": null }
```

`409 collection_exists` if the name is taken. Names are `[A-Za-z0-9_-]{1,64}`. The
collections `purchases`, `price_history`, `price_rules` and `price_offers` are managed by
the pricing subsystem and cannot be created or dropped through this endpoint.

### `GET /v1/collections/{name}`

```json
{ "ok": true, "data": {
  "collection": { "name": "themes", "created_at": 1793817600000, "max_docs": 10000,
                  "default_ttl_ms": null, "schema": { "...": "..." },
                  "indexes": { "by_name": { "field": "name", "unique": false } } },
  "documents": 3 } }
```

### `DELETE /v1/collections/{name}?confirm=true` (admin)

Dropping requires `confirm=true`; without it the call fails with
`400 confirmation_required` and nothing is deleted. Immutable collections refuse
deletion (`403 immutable_collection`).

## Documents

A document is an envelope plus an opaque JSON value:

```json
{ "key": "pastel", "version": 3, "revision": 42,
  "created_at": 1793817600000, "updated_at": 1793817688000, "expires_at": null,
  "value": { "name": "Pastel" } }
```

### `PUT /v1/collections/{c}/documents/{key}`

```json
{ "value": { "name": "Pastel" }, "expected_version": 2, "expires_at": null }
```

* `PUT` answers `200`, `POST /v1/collections/{c}/documents` (key in the body) answers
  `201`; both return `{"collection":"themes","key":"pastel","version":3}`.
* `expected_version: 0` means "must not exist yet"; omitting it means unconditional.
* `409 conflict` (`retryable: false`) when the current version differs; re-read and
  reconcile before issuing a new write.
* `400 validation_failed` when the collection schema rejects the value.
* `422 limit_exceeded` when the document or the collection exceeds configured limits.

### `POST /v1/collections/{c}/documents`

```json
{ "key": "pastel", "value": { "name": "Pastel" }, "expected_version": 1 }
```

Same semantics; key comes from the body. Prefer `PUT` for new code.

### `GET /v1/collections/{c}/documents/{key}`

Returns the envelope above, or `404 not_found`.

### `DELETE /v1/collections/{c}/documents/{key}[?expected_version=N]`

`{"deleted": "k"}`; `409 conflict` if `expected_version` is stale, `404` if unknown.

### `GET /v1/collections/{c}/documents?prefix=P&limit=N&offset=O&reverse=true`

```json
{ "ok": true, "data": { "collection": "themes", "count": 2, "documents": [ … ] } }
```

Keys are returned in lexicographic order (`reverse=true` flips it). `limit` defaults to
100 and is capped at 1000, `offset` skips that many matching keys. An empty page means
"no more keys"; this is not a snapshot-consistent cursor, so a concurrent write can
shift the window.

## Batches

### `POST /v1/collections/{c}/batch`

```json
{ "operations": [
  { "op": "put", "key": "a", "value": {"n": 1}, "expected_version": 0 },
  { "op": "delete", "key": "b" }
] }
```

```json
{ "ok": true, "data": { "operations": 2, "seq": 43 } }
```

All operations succeed or none is applied (one WAL record). A failing operation returns
its own status (`409`, `400`, …) and **no** operation of the batch is applied. A batch
holds at most 1000 operations (`422 limit_exceeded`). The key `ops` is accepted as an
alias for `operations`.

## Transactions

### `POST /v1/transactions`

A transaction uses the same operation list as a batch, but each operation names its own
collection, so one atomic commit can span collections:

```json
{ "operations": [
  { "op": "put",    "collection": "purchases",     "key": "p_1", "value": {"…": "…"} },
  { "op": "put",    "collection": "price_offers",  "key": "of_1", "value": {"…": "…"}, "expected_version": 1 },
  { "op": "delete", "collection": "shop_catalogs", "key": "old" }
] }
```

```json
{ "ok": true, "data": { "operations": 3, "seq": 44 } }
```

Semantics: all operations are validated against the current versions first
(compare-and-set via `expected_version`), then applied as a single WAL record. If any
check fails the whole transaction is rejected with `409 conflict` (`retryable: false`)
and nothing changes. There is no interactive session and no server-side retry loop —
the caller re-reads, reconciles the intended change, and issues a new request. The
Rust `Engine::begin()` API additionally offers a read-your-writes transaction handle
for in-process use.

## Indexes

### `POST /v1/collections/{c}/indexes` (admin)

```json
{ "name": "by_price", "field": "price", "unique": false }
```

### `DELETE /v1/collections/{c}/indexes/{name}` (admin)

### `GET /v1/collections/{c}/indexes/{name}?value=V` — equality lookup

### `GET /v1/collections/{c}/indexes/{name}?from=A&to=B&limit=N` — inclusive range

```json
{ "ok": true, "data": { "rows": [ { "key": "dress", "value": { "price": 50 } } ], "count": 1 } }
```

`value`, `from` and `to` are JSON-encoded (e.g. `?value=50`, `?from=%22a%22`), `limit`
defaults to 100 and is capped at 1000. An equality lookup returns `documents`, a range
lookup returns `rows`.

## Administration

| Endpoint | Role | Effect |
| --- | --- | --- |
| `GET /v1/stats` | read | counts, sizes, per-collection stats |
| `GET /v1/metrics` | read | counters since start (requests, writes, rate-limit rejections) |
| `POST /v1/admin/checkpoint` | admin | WAL → snapshot, returns generation and sequence |
| `POST /v1/admin/compact` | admin | sweep expired docs, repack snapshot, delete retired segments |
| `POST /v1/admin/verify` | admin | checksum every segment/snapshot; answers `200` with `data.ok=true`, or `409` with `data.ok=false` and the issue list |
| `POST /v1/admin/backup` | admin | `{"path":"/backups/x.bak","checkpoint":true}` → `{archive, archive_sha256, content_sha256, entries, bytes, checkpoint_taken, created_at}` |
| `GET /v1/admin/recovery` | admin | recovery reports from the last start |
| `POST /v1/admin/sweep-expired` | admin | run the TTL sweeper now |
| `GET /v1/admin/auth` | admin | users and tokens (never secrets) |

## Personality model

| Endpoint | Role | Effect |
| --- | --- | --- |
| `POST /v1/personality/bootstrap` | admin | create the 14 model collections (idempotent) |
| `GET /v1/personality/overview` | read | per-collection counts and schema presence |
| `GET /v1/personality/profiles` | read | all `personality_profiles` documents |

See [DATA-MODEL.md](DATA-MODEL.md).

## Pricing

| Endpoint | Role | Effect |
| --- | --- | --- |
| `GET /v1/pricing/rules` | read | all rules |
| `GET /v1/pricing/rules/{item}` | read | one rule |
| `PUT /v1/pricing/rules/{item}` | write | create/replace a rule; bumps `version` |
| `POST /v1/pricing/rules/{item}/tick` | write | `{"steps":5}` random walk within bounds |
| `POST /v1/pricing/offers` | write | `{"item_id":"skirt","ttl_ms":60000}` → `{offer_id, price_cents, rule_version, expires_at}` |
| `GET /v1/pricing/offers/{id}` | read | offer state |
| `POST /v1/pricing/purchases` | write | `{"offer_id":"…","quantity":2}` → append-only purchase record |
| `GET /v1/pricing/history/{item}` | read | price history for one item |
| `GET /v1/pricing/audit` | admin | audit log entries |
| `GET /v1/pricing/audit/verify` | admin | verify the audit hash chain |

The pricing audit file and database are separate writes, not one transaction. A
pricing mutation may already be committed if an audit append fails; in that case
the server returns `503 audit_unavailable` and clients must inspect the operation's
state before retrying. Further pricing mutations are rejected until the audit is
reopened by restarting the server after the cause is resolved. A valid suffix removed
from the audit file before the first open cannot be detected without an external
trusted anchor. Each audit record is limited to 1 MiB and parsed with bounded JSON
limits. The full chain is verified while streaming on open and on explicit verify; the
server retains only a recent tail bounded by 1,000 entries and 4 MiB of serialized
records. `GET /v1/pricing/audit` can return fewer entries than requested when the byte
bound is reached. The verify response reports the total verified entry count.

If the audit log cannot be opened at startup, the server records the diagnostic,
continues to serve non-pricing operations, and rejects pricing mutations. The server
logs the initialization failure; an administrator can also inspect `pricing_audit`
in `GET /v1/admin/recovery`, which reports availability and the startup diagnostic.
| `POST /v1/pricing/simulate` | read | `{"rule":{…},"steps":10,"seed":"x"}` → deterministic history, nothing stored |

See [DYNAMIC-PRICING.md](DYNAMIC-PRICING.md) for invariants.

## Error codes

| Code | Status | Retryable | Meaning |
| --- | --- | --- | --- |
| `authentication_required` | 401 | no | no `Authorization` header |
| `invalid_token` | 401 | no | unknown or revoked token |
| `invalid_credentials` | 401 | no | wrong username/password |
| `forbidden` | 403 | no | role too low for this operation |
| `forbidden_collection` | 403 | no | economy/pricing collections are managed internally |
| `immutable_collection` | 403 | no | `purchases` and `price_history` are append-only |
| `read_only` | 403 | no | server started with `--read-only` |
| `not_found` / `collection_not_found` / `index_not_found` | 404 | no | missing object |
| `unknown_endpoint` | 404 | no | no route matched |
| `conflict` | 409 | **yes** | version/commit conflict or duplicate name |
| `collection_exists` / `index_exists` | 409 | no | name already taken |
| `offer_stale` / `offer_expired` / `offer_already_redeemed` | 409 | no | pricing state changed |
| `invalid_request` / `invalid_json` / `invalid_query` | 400 | no | malformed request |
| `validation_failed` | 400 | no | schema or field validation failed |
| `body_too_large` | 413 | no | body over `max_body_bytes` |
| `limit_exceeded` | 422 | no | document/collection limit reached |
| `rate_limited` | 429 | **yes** | slow down, `Retry-After` is set |
| `database_locked` | 503 | **yes** | another process owns the directory |
| `commit_outcome_unknown` | 503 | no | WAL append/sync failed after the mutation may have reached the log; reopen and inspect before retrying |
| `restore_outcome_unknown` | 503 | no | restore target was renamed into place but syncing its parent failed; inspect the target and retained `previous` directory before retrying |
| `recovery_required` | 503 | no | an interrupted restore intent blocks database open until target/staging/previous paths are inspected |
| `internal` / `io_error` / `corrupt_data` | 500 | no | server-side failure; `corrupt_data` means a checksum failed |
| `unsupported` | 501 | no | feature explicitly not implemented |

## Rate limiting and limits

* Default 50 requests/second with a burst of 100, per credential (per client IP for
  anonymous traffic). Excess requests get `429 rate_limited` with `Retry-After: 1`.
* `max_body_bytes` defaults to 4 MiB (`413 body_too_large`).
* Document size, per-collection document count and batch size are bounded per collection
  (`422 limit_exceeded`).
* Only `GET`, `POST`, `PUT`, `DELETE` are supported; `HEAD`/`OPTIONS` answer `404`.

## Concurrency checklist for clients

1. Read the document, remember `version`.
2. Write with `expected_version`.
3. On `409 conflict` (`retryable: false`), re-read and reconcile the intended change;
   do not replay the same request automatically.
4. On `429` (`retryable: true`), retry within a bound and honour `Retry-After`.
