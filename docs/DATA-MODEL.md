# Personality and shop data model

PreppyDB is a general document store; the personality/shop model is an **optional layer**
on top of it, defined in the `preppydb-model` crate. Nothing in the storage engine knows
about it, and a database that never calls `POST /v1/personality/bootstrap` or
`preppydb model bootstrap` is a plain document store.

The model exists so that a personality application gets schemas, indexes and limits
without inventing them per deployment, and so that the same definitions can be reviewed
once instead of re-derived in every client.

## Bootstrapping

```bash
preppydb model bootstrap --data-dir ./data          # CLI, one-shot
curl -X POST -H "Authorization: Bearer $TOKEN" localhost:7070/v1/personality/bootstrap
```

Both are idempotent: existing collections are left untouched (a local schema change is
never silently reverted), missing ones are created with their schema and indexes.

## Collections

| Collection | Holds | Required fields | Indexes | Max docs |
| --- | --- | --- | --- | --- |
| `personality_profiles` | One record per personality version | `name`, `version` | `by_name` | 1 000 |
| `memories` | Episodic/semantic memories | `profile_id`, `kind`, `content` | `by_profile`, `by_kind` | 2 000 000 |
| `preferences` | Likes/dislikes with weights | `profile_id`, `subject`, `value` | `by_subject` | 1 000 000 |
| `virtual_items` | Item definitions (catalog objects) | `name`, `kind` | `by_rarity`, `by_kind` | 500 000 |
| `virtual_collections` | Named sets of items owned by a profile | `name` | `by_owner` | 100 000 |
| `diary_entries` | Diary/journal entries | `profile_id`, `title`, `body`, `created_at` | `by_profile`, `by_created` | 1 000 000 |
| `events` | One-off events (calendar entries) | `title`, `starts_at` | `by_starts` | 1 000 000 |
| `schedules` | Recurring weekly schedules | `name`, `entries` | — | 10 000 |
| `themes` | Design/makeover themes (palette, CSS variables) | `name`, `palette` | — | 10 000 |
| `makeover_history` | Applied theme changes per profile | `profile_id`, `applied_at` | `by_profile` | 1 000 000 |
| `shop_catalogs` | Shop catalogs with item/price lists | `name`, `items` | — | 1 000 |
| `price_rules` | Dynamic pricing rules per item | `item_id`, `min_price_cents`, `max_price_cents`, `anchor_price_cents` | `by_item` | 100 000 |
| `price_history` | Append-only price changes | `item_id`, `price_cents`, `rule_version`, `at` | `by_item` | 5 000 000 |
| `purchases` | Append-only purchase records | `item_id`, `price_cents`, `purchased_at`, `quantity` | `by_item` | 5 000 000 |

Every schema uses `"additionalProperties": true`, so an application may store extra
fields (e.g. Molly-specific attributes) without a migration. The typed fields are
validated on write with types, bounds, lengths and enumerations.

### Field notes

* `personality_profiles.version` is the profile revision chosen by the application; it is
  independent of the storage `version` that implements compare-and-set.
* `memories.kind` ∈ `episodic | semantic | preference | goal | note`; `importance` is
  0–100; `content` is capped at 20 000 characters, which keeps one memory small enough to
  embed in a prompt.
* `preferences.weight` is 0–100 so clients can rank without re-normalising.
* `diary_entries.body` is capped at 100 000 characters; `private` marks entries that a
  UI must not display in shared contexts (the database does not enforce visibility — it
  is a client-side policy flag).
* `schedules.entries` holds at most 512 entries, each `{day: 0–6, start_minute: 0–1439,
  end_minute: 0–1439, activity: string}`.
* `themes.palette` and `themes.css_variables` are free-form objects so themes can carry
  arbitrary colour systems; `makeover_history.changes` records the diff that was applied.
* `shop_catalogs.items` holds up to 10 000 `{sku, name, price_cents, stock}` entries with
  `price_cents >= 1`.

### Append-only collections

`purchases` and `price_history` are **immutable**:

* overwriting an existing key returns `409 immutable_collection`;
* deleting any key returns `403 immutable_collection`;
* dropping the collection requires admin rights and is refused by the API router.

This is what makes "completed purchases are never mutated" enforceable rather than a
convention.

### Forbidden collection names

The API refuses to create `economy_balances`, `wallet`, `wallets`, `balance`, `balances`,
`credit_ledger` and `debit_ledger` (`403 forbidden_collection`). PreppyDB does not manage
currency balances; an economy belongs to the application that owns it, and a database
that silently accepted writes there could corrupt it.

## How an application is expected to use it

The database is deliberately passive. A personality application (Molly, later) is
expected to:

1. store one profile document per personality version, keeping the previous one so an
   upgrade can be rolled back;
2. append memories with `kind` and `importance`, and query them with
   `GET /v1/collections/memories/indexes/by_profile?value="molly-v3"`;
3. keep diary entries separate from memories so retention policies can differ;
4. resolve today's theme through `themes` and record each change in `makeover_history`;
5. read prices from `price_rules`/`price_history` and write purchases through the pricing
   API only (`POST /v1/pricing/purchases`), never by writing to `purchases` directly.

Nothing in PreppyDB calls into Molly, and nothing in Molly is required for these
collections to work — the model is usable standalone, as the integration tests do.

## Limits worth knowing

* Documents are limited to 1 MiB by default (`EngineConfig::limits`), collections to the
  `max_docs` values above, batches to 1 000 operations, scans to 1 000 keys per page.
* `personality_profiles` and `shop_catalogs` are intentionally small; if an application
  needs more, it should raise `max_docs` explicitly rather than rely on the default.
* Deletes in immutable collections are refused, so growth is bounded only by `max_docs`;
  plan retention with a separate export if the cap is approached.
