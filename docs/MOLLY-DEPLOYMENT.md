# PreppyDB next to SeaWorld DB on a Raspberry Pi 4

Operator notes for running PreppyDB as an **additional** database for Molly
(ARM64, Debian 13, Docker). SeaWorld DB stays Molly's authoritative store until a
separate cutover decision; nothing here changes what Molly reads today.

Nothing in this document was executed in the repository's test environment: there
is no Docker daemon and no ARM64 host there. Treat it as a configuration to be
validated on the device, not as a report of a working deployment.

## What the container guarantees

| Property | How |
| --- | --- |
| Durable acknowledgements | `PREPPYDB_SYNC_MODE=always` (the entrypoint default); `/v1/version` reports `durable_writes` |
| Data survives recreation | named volume `preppydb-data` → `/var/lib/preppydb` |
| Survives reboot | `restart: unless-stopped` |
| Not reachable from the network | port published on `127.0.0.1` only |
| No secrets in images or compose | `PREPPYDB_ADMIN_PASSWORD` / `PREPPYDB_BOOTSTRAP_TOKEN` come from the environment, never from a file in Git |
| Health visible to Docker | `preppydb health --url http://127.0.0.1:7077` every 30 s |
| Clean shutdown | `tini` as PID 1, `stop_grace_period: 35s` so an in-flight `fsync` finishes |

`PREPPYDB_SYNC_MODE` accepts `always`, `interval`, `interval:<ms>` or `never`;
any other value makes the container exit 64 with a message.

If the container is started with its own `--config <file>` (profiles, query
planning, limits), the entrypoint does **not** add a second one — the operator's
file wins and `sync_mode` has to be in it. Setting the environment variable in
that case prints a warning and changes nothing, because silently overriding a
config file would drop the rest of its settings. Do not use anything
but `always` for a database an adapter writes to: with `interval` and `never`
the server may acknowledge a write that a power loss still loses.

## ARM64

The `Dockerfile` is a multi-stage build that compiles from source; build it on the
Pi itself (native `arm64`) or use the `linux/arm64` image built by CI. There is no
prebuilt image registry entry to pull, so:

```sh
# on the Pi, in the PreppyDB checkout
docker build -f docker/Dockerfile -t preppydb:local .
```

A native build on a Pi 4 takes a long time (tens of minutes); build once, keep
the image.

## First start

```sh
cd /srv/preppydb                      # a checkout or a copy of compose + .env
export PREPPYDB_ADMIN_USER=admin
export PREPPYDB_ADMIN_PASSWORD='at-least-12-characters'   # first start only
docker compose up -d
docker compose ps                     # wait for (healthy)
curl -s http://127.0.0.1:7077/healthz
curl -s http://127.0.0.1:7077/v1/version
```

`/v1/version` must contain `"sync_mode":"always"` and `"durable_writes":true`.
If `durable_writes` is missing, the image predates that field; the Molly session
still accepts `sync_mode: "always"`, but prefer an image that reports the flag.

`PREPPYDB_ADMIN_PASSWORD` is only read while `auth.json` does not exist on the
volume. After that it is ignored; keep it out of the environment to avoid
confusion.

## Stopping, restarting, recovery

```sh
docker compose stop          # graceful: SIGTERM → in-flight request → clean engine shutdown
docker compose start
docker compose restart
docker compose logs -f --tail 100
```

If the server reported `commit_outcome_unknown` (a write whose fsync outcome is
unknown), it stays poisoned — every request, including reads, fails — until the
process is restarted:

```sh
docker compose restart
docker exec -it preppydb preppydb verify --data-dir /var/lib/preppydb
# What the last start found (admin token in PREPPYDB_TOKEN on the host shell):
curl -s -H "Authorization: Bearer $PREPPYDB_TOKEN" http://127.0.0.1:7077/v1/admin/recovery
```

That is intentional: a client that retries after an unknown outcome risks a
double booking. Restart, then reconcile the one document that was in flight.

## Backup

```sh
docker exec preppydb preppydb backup --data-dir /var/lib/preppydb \
    --out /var/lib/preppydb/backups/$(date +%F).bak
docker cp preppydb:/var/lib/preppydb/backups/$(date +%F).bak ./preppydb-$(date +%F).bak
docker exec preppydb preppydb backup-verify --archive /var/lib/preppydb/backups/$(date +%F).bak
```

Copy the archive off the volume; a backup inside the same volume is not a backup.
`preppydb backup-verify --archive …` checks it.

## Token for Molly

Create a token once and put it in the environment (systemd `EnvironmentFile` or
`/etc/molly/molly.env`, mode 0600), never in a compose file:

```sh
docker exec preppydb preppydb token create --name molly --role write \
    --data-dir /var/lib/preppydb
```

The token is printed once. On the Molly side:

```sh
MOLLY_PREPPYDB_ENABLED=false          # stays false until an operator switches
MOLLY_STORAGE_BACKEND=seaworld        # seaworld until the migration verified
MOLLY_PREPPYDB_REQUIRE_MIGRATED=true  # never re-create an unmigrated guild
MOLLY_PREPPYDB_URL=http://127.0.0.1:7077
MOLLY_PREPPYDB_TOKEN=…
MOLLY_PREPPYDB_TIMEOUT_MS=5000
MOLLY_PREPPYDB_COLLECTION=molly_guilds
MOLLY_PREPPYDB_REQUIRE_DURABLE=true
```

Molly runs as a host service (`ops/molly.service`), not in compose, so
`127.0.0.1:7077` is reachable for it without exposing the port anywhere else. If
Molly is ever containerised, put both containers on one user-defined network and
stop publishing the port at all.

## Resource expectations on a Pi 4

* One `fsync` per commit and a single writer thread: expect roughly 5–20 ms per
  document write on SD/USB storage, so a whole-guild document rewrite costs that
  per operation. Measure before routing a chat command that writes on every
  message.
* Memory: the engine keeps the working set plus WAL buffers; a guild document is
  only resident after it is read. Budget from real document sizes
  (`migration_summary.largest_bytes` from the Molly migration tool).
* The volume should live on a real SSD or a good microSD with `noatime`; the WAL
  is fsynced constantly in `always` mode.

## Rollback

Nothing in this setup changes Molly's data path until a feature is routed. To
roll back: point Molly back at SeaWorld (`kind = "seaworld"`, the default), stop
the container, keep the volume for inspection. No reverse migration is needed —
SeaWorld was never written to by this deployment.
