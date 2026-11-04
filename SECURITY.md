# Security policy

## Reporting a vulnerability

PreppyDB is a pre-release project. Please **do not** open a public issue for a suspected
vulnerability. Report it privately to the repository maintainer (GitHub private
vulnerability report or a direct message to `@Reina-Kousaka1`) with:

* the affected version/commit,
* a reproduction (commands, request bodies, data directory if relevant),
* the impact you believe it has.

You can expect an acknowledgement within a week. As this is a spare-time project there is
no bug bounty and no guaranteed fix deadline; reports are handled in good faith and the
fix is credited in `CHANGELOG.md` unless you ask otherwise.

Supported versions: only the latest commit on `main` is supported while the project is
0.x. Older 0.x versions do not receive backports.

## What the project already does

These controls exist in the code today; each is covered by the tests listed in
[docs/TESTING.md](docs/TESTING.md).

**Authentication and authorisation**

* No default credentials, ever. The server refuses to start without at least one admin
  user or API token and prints instructions instead.
* Passwords: minimum 12 characters, PBKDF2-SHA-256 with per-user salts, constant-time
  comparison.
* API tokens: 32 random bytes from the OS entropy source, stored only as SHA-256 digests,
  printed exactly once, revocable, optional expiry.
* Roles `read` / `write` / `admin` checked per endpoint before routing logic runs.
* Login responses are identical for unknown users and wrong passwords; login attempts are
  rate limited per client address.
* Sessions live in memory only, expire after 8 hours, and vanish on restart.

**Network exposure**

* The server binds to `127.0.0.1` by default and **refuses** a non-loopback bind unless
  `allow_public_bind` is set explicitly (the container entrypoint sets it deliberately
  inside the container network and documents why).
* No TLS in the server: terminate TLS in a reverse proxy (Caddy/nginx/Traefik). The
  README and operations guide say so instead of pretending otherwise.
* Bounded request line, headers, body (`max_body_bytes`, 4 MiB default, 256 MiB maximum)
  and per-credential rate limits; oversized requests are rejected with `413`.
* The admin UI ships a strict CSP, no inline script and keeps the token in
  `sessionStorage`; it never renders server-provided HTML.

**Data safety**

* Every WAL frame and snapshot block carries CRC32C; snapshots carry a SHA-256. A
  checksum failure is reported as `corrupt_data`, never silently ignored.
* Backups are hashed per member and as a whole; `restore` verifies everything before
  writing and moves an existing target aside instead of deleting it.
* Immutable collections (`purchases`, `price_history`) cannot be overwritten or deleted
  through the API, so recorded history cannot be rewritten.
* Economy/balance collection names are rejected outright, so the database can never be
  made responsible for a wallet it does not understand.
* No hard-coded filesystem paths outside the data directory except for the backup
  destination the operator names.
* `auth.json` is written with mode 0600 on Unix; the container runs as uid 10001 with a
  `0700` data directory.

**Operational hygiene**

* One writer per directory, enforced with an OS advisory lock on the `LOCK` file
  (`flock`/`LockFileEx`), so two processes cannot interleave writes and a crash cannot
  leave a stale lock behind.
* No telemetry, no outbound connections, no update checks, no third-party dependencies.
  `cargo tree` shows only workspace members.

## Known limitations and accepted risks

Read these before exposing PreppyDB to anything:

1. **No TLS inside the server.** Plain HTTP only; without a proxy, tokens and data travel
   in the clear.
2. **Hand-written crypto.** SHA-256/HMAC/PBKDF2/CRC32C are implemented in-tree to keep the
   dependency tree empty. They are tested against standard vectors, but they have not had
   the scrutiny of an audited library such as `ring` or `sha2` (RustCrypto).
   Token/salt randomness comes from `/dev/urandom` on Unix; on other platforms (Windows)
   the standard library exposes no OS RNG, so a SHA-256 DRBG seeded from `RandomState`
   (OS-seeded), the clock and the pid is used instead. That is a real, documented
   weakness on Windows: prefer running the server on Linux until an audited RNG is
   wired in.
3. **Hand-written HTTP parser.** Only `GET`/`POST`/`PUT`/`DELETE`, no chunked request
   bodies, `Connection: close` per request. It is intentionally minimal, which reduces
   attack surface but also means it is not battle-tested.
4. **Tokens are bearer tokens.** Anyone who obtains one has its role until it is revoked;
   there is no per-request signing, mTLS or IP binding.
5. **No audit of API access.** The pricing audit log covers pricing events only; HTTP
   access is logged to stderr by the supervisor, not by the database.
6. **Single-node.** There is no replication, so a host loss means restoring a backup.
7. **Denial of service is only partially mitigated.** Rate limiting and body limits apply,
   but a client with a valid token can still fill the disk with documents until `max_docs`
   is reached; size collections deliberately.
8. **Memory-resident working set.** A dataset larger than RAM degrades the host; there is
   no admission control beyond collection limits.
9. **The web UI has no CSRF token.** It authenticates with a bearer token that must be
   pasted in, which browsers do not attach automatically, and the API accepts only
   `Authorization` headers — so classic CSRF does not apply, but this is a deliberate
   design choice rather than an audited guarantee.

## Guidance for operators

* Bind to loopback; put a TLS-terminating proxy in front for remote access.
* Use per-client tokens with the smallest role that works, and set expiry dates.
* Keep the admin API off the public internet.
* Back up regularly (`preppydb backup`), verify archives (`backup-verify`) and run
  `preppydb verify` after every restore.
* Keep the data directory on a filesystem you trust (no network filesystems with broken
  `fsync` semantics) and use `sync_mode: "always"` for data you cannot lose.
* Watch `/readyz`, `preppydb verify` and disk usage.

## Out of scope

Deployment to production, changes to other projects (Molly, SeaWorld, Tori, Noélia), and
publishing container images or releases — those need an explicit maintainer decision.
