# PreppyDB

A standalone, embedded-first NoSQL database written in Rust — *molly's own db* (the
original one-line description of this repository, kept here on purpose). It stores JSON-like
documents in named collections, is durable through a write-ahead log with checkpoints
and crash recovery, speaks a versioned HTTP API, ships a web admin UI and an admin CLI,
and includes a first-class data model for a personality/shop application (memories,
diary entries, themes, catalogs) plus a bounded, audited dynamic-pricing engine.

PreppyDB is technically independent: it has no dependency on Molly, SeaWorld, Tori or
Noélia. It is *intended* to become Molly's persistent personality store, but the
database itself knows nothing about any of them.

> **Status: 0.1.0, pre-release.** Everything described below is implemented and tested
> on Linux x86-64. Replication/clustering, TLS termination and the Windows/ARM64/Docker
> runs are *not* done here — each is listed honestly in
> [docs/TESTING.md](docs/TESTING.md#what-was-not-executed) and
> [## Not implemented](#not-implemented-yet).

---

## Quick start

### Docker (recommended)

```bash
export PREPPYDB_ADMIN_PASSWORD='choose-a-long-password'   # min. 12 characters
docker compose up -d --build
curl -s http://127.0.0.1:7077/healthz
```

The password is used exactly once, to create the first admin user on an empty volume.
Afterwards credentials live in `auth.json` inside the volume. The container refuses to
start when it has neither a credential file nor a configured password — there is no
default password anywhere.

Pre-built images are **not** published; `docker compose` builds locally. To build an
image for another architecture:

```bash
docker buildx build --platform linux/arm64 -f docker/Dockerfile -t preppydb:arm64 .
```

### Native (Linux, macOS, Windows)

```bash
cargo build --release
./target/release/preppydb-server --data-dir ./data --init-admin admin --init-only  # password from $PREPPYDB_ADMIN_PASSWORD
./target/release/preppydb-server --data-dir ./data --listen 127.0.0.1:7070
```

See [docs/OPERATIONS.md](docs/OPERATIONS.md) for Windows (PowerShell), Raspberry Pi and
systemd instructions.

### First requests

```bash
# create an API token (printed once)
./target/release/preppydb-server --data-dir ./data --bootstrap-token dev --init-only

TOKEN=...                       # the token printed above
curl -s -X POST localhost:7070/v1/collections \
     -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"name":"themes"}'

curl -s -X PUT localhost:7070/v1/collections/themes/documents/pastel \
     -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"value":{"name":"Pastel","palette":{"primary":"#f6c1d0"}}}'

curl -s localhost:7070/v1/collections/themes/documents/pastel \
     -H "Authorization: Bearer $TOKEN"
```

The web UI is served at `http://127.0.0.1:7070/`; paste a token into it and manage
collections, documents, pricing and backups from there.

---

## What is implemented

| Area | State |
| --- | --- |
| Document store: collections, CRUD, scan by key prefix, CAS versions | done |
| Atomic batches and read-committed transactions | done |
| Write-ahead log, group `fsync`, checkpointing, atomic manifest, recovery | done |
| Compaction, orphan-segment cleanup, TTL + expiry sweeper | done |
| CRC32C on every WAL frame and snapshot block, SHA-256 on snapshots, `verify` | done |
| Secondary indexes (equality, range, unique) | done, with documented limits |
| Consistent snapshots + `backup`/`restore` with integrity checks | done |
| Versioned HTTP API (`/v1`), structured errors, request ids | done |
| Bearer tokens, users, sessions, roles (read/write/admin), rate limiting | done |
| Web admin UI (collections, documents, stats, pricing, backup, themes) | done |
| Admin/diagnostics CLI (`preppydb`) | done |
| Rust client library + Lua/Luvit client | done |
| Personality/shop data model (14 collections with schemas and indexes) | done |
| Dynamic pricing engine with bounds, offers, purchases, audit log | done |
| Docker (multi-stage, amd64 + arm64) and compose example | written, **not** built here |
| CI for Linux x86-64, Windows, ARM64 (qemu), Lua, Docker | written, **not** run here |
| Replication / clustering / sharding | **not started** (later module) |

---

## Repository layout

```
crates/preppydb-json      dependency-free JSON value, parser, canonical form
crates/preppydb-crypto    SHA-256, HMAC, PBKDF2, CRC32C, base64, hex, RNG (std only)
crates/preppydb-storage   engine: records, WAL, snapshots, catalog, txn, backup
crates/preppydb-pricing   price rules, offers, purchases, audit log
crates/preppydb-model     personality/shop collection model
crates/preppydb-server    HTTP server, auth, config, rate limiting, web UI
crates/preppydb-client    blocking Rust client library
crates/preppydb-cli       `preppydb` admin/diagnostics CLI
clients/luvit             Lua/Luvit client + tests
docker/                   Dockerfile, entrypoint, .env example
scripts/                  crash-recovery and Docker-persistence test scripts
docs/                     architecture, API, operations, testing, model, pricing
```

The whole workspace has **no third-party dependencies**: everything is `std` plus this
repository's own crates. That keeps builds offline-capable and the supply chain empty,
at the cost of hand-written JSON/crypto primitives (see
[docs/ARCHITECTURE.md#why-no-dependencies](docs/ARCHITECTURE.md)).

---

## Documentation

| Document | Contents |
| --- | --- |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Crates, on-disk format, WAL/recovery, consistency, design trade-offs |
| [docs/API.md](docs/API.md) | Every endpoint, error codes, auth, examples |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | Docker, native install, systemd, Windows, Pi, backup/restore, upgrades |
| [docs/DATA-MODEL.md](docs/DATA-MODEL.md) | Personality/shop collections and their schemas |
| [docs/DYNAMIC-PRICING.md](docs/DYNAMIC-PRICING.md) | Pricing rules, offers, purchases, audit invariants |
| [docs/CLI.md](docs/CLI.md) | `preppydb` command reference |
| [docs/TESTING.md](docs/TESTING.md) | What is tested, what was executed, what was not |
| [SECURITY.md](SECURITY.md) | Security policy, implemented controls, accepted risks |
| [CHANGELOG.md](CHANGELOG.md) | What 0.1.0 contains, what is unverified, what is missing |
| [clients/luvit/README.md](clients/luvit/README.md) | Lua client usage and test setup |

---

## Design in one page

* **Storage.** Documents are `key -> JSON value` records with a monotonically increasing
  version and a per-key revision (the global sequence number of the write). Every
  mutation is appended to a WAL segment as a length-prefixed, CRC32C-protected frame
  before it is applied in memory. A checkpoint rotates the WAL, writes a checksummed
  snapshot, atomically replaces the manifest and deletes retired segments. Recovery
  replays surviving segments after the snapshot sequence number and truncates a torn
  tail with an explicit report instead of silently guessing.
* **Concurrency.** One process owns the directory (advisory lock file). Inside the
  process a writer mutex serialises mutations; readers use the in-memory store under a
  read lock. Transactions are read-committed with read-your-writes buffering and
  version validation at commit; there is no automatic retry, conflicts surface as
  `409 conflict` with `retryable: true`.
* **API.** `GET/POST/PUT/DELETE /v1/...`, JSON in and out, `{"ok":true,"data":...}` or
  `{"ok":false,"error":{"code","message","retryable"}}`. Bearer tokens or session
  tokens, per-credential rate limiting, bounded request bodies, request ids.
* **Personality model.** 14 documented collections (`personality_profiles`, `memories`,
  `preferences`, `virtual_items`, `virtual_collections`, `diary_entries`, `events`,
  `schedules`, `themes`, `makeover_history`, `shop_catalogs`, `price_rules`,
  `price_history`, `purchases`) with JSON-Schema validation, size limits and indexes.
* **Pricing.** Rules are bounded (`min_price_cents >= 1`, anchor inside bounds, step
  limits, discounts 1–90 %), price changes are deterministic from a seed for tests,
  offers are bound to a rule version and die on rule edits, purchases are append-only
  and cannot be mutated or deleted through the API.

## Not implemented yet

* Replication, clustering, sharding, failover — a later module; nothing here pretends
  otherwise.
* TLS in the server itself: terminate TLS in a reverse proxy (Caddy/nginx/Traefik).
* Multi-process concurrent writers on one directory (one writer per directory, enforced).
* Range scans by key order across a whole collection (prefix scan only); index ranges
  are supported.
* Automatic vacuuming/retention beyond TTL sweeps and compaction.
* Published container images or GitHub releases — both need explicit maintainer approval.

## License

AGPL-3.0-or-later, as declared by the existing [`LICENSE`](LICENSE) file. No other
licence was introduced.
