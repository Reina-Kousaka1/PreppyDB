# `preppydb` CLI reference

The CLI works **directly on a data directory**. It takes the same single-writer lock as
the server, so it refuses to run while a server has that directory open — that is
deliberate: maintenance never races a live writer, and a backup is always taken from a
consistent state.

Every command prints JSON on stdout (pretty-printed), diagnostics on stderr.

```
preppydb <command> [options]
```

Exit codes: `0` success, `1` command failed, `2` usage error.

## Global options

| Option | Effect |
| --- | --- |
| `--data-dir <PATH>` | database directory (required by every local command) |
| `--read-only` | open read-only where supported |
| `--repair` | allow recovery to truncate a corrupt WAL tail (**destructive**) |

## Credentials

```bash
preppydb init --data-dir ./data [--bootstrap-model]
preppydb token create --data-dir ./data --name ci --role write --expires-days 30
preppydb token list   --data-dir ./data
preppydb token revoke --data-dir ./data --id tok_…
preppydb user create  --data-dir ./data --username molly --role write   # password on stdin
preppydb user list    --data-dir ./data
preppydb user disable --data-dir ./data --username molly
```

`token create` prints the plaintext exactly once; only its SHA-256 digest is stored.
`--bootstrap-model` also creates the 14 personality/shop collections.

## Data

```bash
preppydb stats --data-dir ./data
preppydb collections --data-dir ./data
preppydb collection info   --data-dir ./data --name themes
preppydb collection create --data-dir ./data --name notes [--schema-file schema.json] [--max-docs 1000]
preppydb collection drop   --data-dir ./data --name notes --confirm
preppydb index create --data-dir ./data --collection notes --name by_tag --field tag [--unique]
preppydb index drop   --data-dir ./data --collection notes --name by_tag

preppydb get    --data-dir ./data --collection themes --key pastel
preppydb put    --data-dir ./data --collection themes --key pastel --value '{"name":"Pastel"}'
preppydb put    --data-dir ./data --collection themes --key pastel --value-file pastel.json
preppydb put    --data-dir ./data --collection themes --key pastel --value '{"name":"x"}' --expected-version 3
preppydb delete --data-dir ./data --collection themes --key pastel [--expected-version 3]
preppydb scan   --data-dir ./data --collection themes [--prefix p] [--limit 50]
preppydb batch  --data-dir ./data --file ops.json
```

`ops.json` for `batch` is a JSON array:

```json
[ { "op": "put", "collection": "themes", "key": "pastel", "value": {"name": "Pastel"} },
  { "op": "delete", "collection": "themes", "key": "old" } ]
```

A failing operation aborts the whole batch (`--expected-version` enables compare-and-set).

## Maintenance and diagnostics

```bash
preppydb verify   --data-dir ./data     # CRC32C + SHA-256 over manifest, snapshots and WAL
preppydb checkpoint --data-dir ./data   # WAL → snapshot, rotate segments
preppydb compact  --data-dir ./data     # sweep expired documents and repack
preppydb sweep    --data-dir ./data     # expire documents now
preppydb wal      --data-dir ./data     # per-segment: records, valid bytes, torn tail, corruption
preppydb backup   --data-dir ./data --out /backups/x.bak [--no-checkpoint]
preppydb backup-verify --archive /backups/x.bak
preppydb restore  --archive /backups/x.bak --target /srv/restored [--force]
preppydb model bootstrap --data-dir ./data
```

`verify` answers `{"ok": true, "issues": [], "segments": [...], "snapshot": {...}}` and
exits `1` when it found a problem. `restore` verifies every checksum *before* writing and
moves an existing target to `<target>.replaced-<ts>` instead of deleting it.

## Pricing

```bash
preppydb pricing simulate --min 1000 --max 2000 --anchor 1500 \
                          --steps 20 --seed demo [--step-cents 50] [--discount 15]
preppydb audit verify --data-dir ./data          # verify the audit hash chain
preppydb audit show   --data-dir ./data [--file PATH]
```

`pricing simulate` is pure computation: it validates the bounds, runs a seeded walk and
prints the resulting history. It never touches a database.

## Remote

```bash
preppydb health --url http://127.0.0.1:7070 [--token TOKEN] [--timeout 3000]
```

Probes `/healthz` and `/readyz` on a **running** server (no `--data-dir` needed) and exits
`1` when the server is unreachable or unhealthy. This is what the Docker `HEALTHCHECK`
uses. The token can also come from `$PREPPYDB_TOKEN`; the health endpoints are public, so
it is optional.

## Notes

* Paths: `--data-dir` accepts POSIX and Windows paths; quote paths containing spaces.
* All mutating commands are durable (`fsync`) before they print their result.
* `get` prints the full envelope (`key`, `value`, `version`, timestamps); pipe through
  `jq '.value'` to see just the document.
