# Dynamic pricing

The pricing engine (`preppydb-pricing`) is a small, bounded, audited system for changing
prices over time and selling items at a price the customer was quoted. It has no
relationship with any wallet or balance: a purchase records what was charged, and an
economy is the application's business.

## Model

```rust
PriceRule { item_id, min_price_cents, max_price_cents, step_cents, anchor_price_cents,
            floor_price_cents, discounts: [DiscountRule], version, updated_at, enabled }

DiscountRule { label, percent (1..=90), starts_at: Option<i64>, ends_at: Option<i64> }

PriceChange { item_id, previous_cents, new_cents, discounted_cents, rule_version, at,
              sample, applied_discounts, clamped }

PriceOffer { offer_id, item_id, price_cents, rule_version, created_at, expires_at, redeemed }

Purchase   { purchase_id, item_id, offer_id, price_cents, rule_version, purchased_at, quantity }
```

All amounts are integer cents. There is no floating point anywhere in the money path.

## Rule validation (rejected at write time)

* `item_id` must not be empty.
* `min_price_cents >= 1` — **prices are never zero or negative**.
* `max_price_cents >= min_price_cents`.
* `anchor_price_cents` must lie inside `[min, max]`.
* `step_cents >= 0` and `floor_price_cents >= 1`; `floor_price_cents <= min_price_cents`
  (the floor is a second, independent lower bound used by discounts).
* `max_price_cents <= 100_000_000` (sanity bound, 1 M currency units).
* `version >= 1`.
* A discount percentage must be 1–90 and its time window must not be inverted.

Every computation path calls `rule.clamp()`, so even a bug elsewhere cannot produce a
price outside `[max(min,1), max]`.

## Price changes

`next_price(rule, current, now)`:

1. clamp `current` into the bounds;
2. draw one 64-bit sample from the seeded generator;
3. map it to `[-step, +step]` and add it to the current price;
4. clamp to the bounds again (`clamped` flag records whether it was necessary);
5. apply every active discount in order, never going below `floor_price_cents`;
6. return a `PriceChange` containing the raw sample, so the decision can be replayed.

`version` is incremented for each accepted change, and each change is appended to
`price_history` (append-only).

### Determinism

`PricingEngine::with_seed(seed)` and the server's `pricing_seed` make the random walk
reproducible: the same seed and the same starting state always produce the same
sequence. This is what the tests and `POST /v1/pricing/simulate` rely on. With no seed the
engine uses the OS entropy source and reports a fingerprint of the seed in
`GET /v1/pricing/audit`/stats so an operator can tell which mode is active.

## Offers and purchases

An offer is a time-boxed quote:

* created at the rule's current effective price (anchor + active discounts), never below
  1 cent;
* carries the `rule_version` it was created for;
* optionally expires (`ttl_ms`).

Redemption fails with a specific, non-retryable code when:

| Situation | Code |
| --- | --- |
| offer id unknown | `offer_unknown` |
| offer already redeemed | `offer_already_redeemed` |
| offer expired | `offer_expired` |
| rule was edited after the offer was created | `offer_stale` |
| quantity is 0 or > 10 000 | `invalid_request` / `invalid_rule` |

On success a `Purchase` is written **and** the offer is marked redeemed inside one
transaction, so a crash cannot produce a redeemed offer without a purchase or a purchase
without the offer being consumed.

Purchases live in the immutable `purchases` collection: the API refuses to overwrite an
existing purchase key (`409 immutable_collection`) and refuses deletes
(`403 immutable_collection`). The recorded `price_cents` is exactly what the customer was
quoted, forever.

## Audit log

Each persisted pricing mutation is bracketed by records in
`<data-dir>/pricing-audit.log`. A `pricing_operation_started` record, containing an
operation ID, operation name and exact intended detail, is synchronized before the
database write. A `pricing_operation_committed` record is synchronized after a known
successful write. A definite rejection is recorded as `pricing_operation_aborted`.
Tick requests record each history change and the final current-price update separately.

```
AuditEntry { seq, kind, at, detail, previous_hash, hash }
```

`hash = SHA-256(seq || kind || at || detail || previous_hash)`. `GET /v1/pricing/audit/verify`
(or `preppydb audit verify`) recomputes the chain; editing, reordering or deleting an
entry breaks it and is reported instead of being ignored. The log is append-only from the
server's point of view — there is no rotate/truncate endpoint.

If the process stops after a start record but before its terminal record, the outcome
is unresolved. On restart, the admin recovery report lists that intent and pricing
mutations are blocked with `503 audit_outcome_unknown` until an operator inspects the
database and audit state. The system does not infer success, invent a terminal record,
or retry the operation. If writing the start record fails, the database mutation does
not begin.

Audit records are streamed and verified one at a time on open and explicit verify; a
record is limited to 1 MiB and parsed with bounded JSON depth/item limits. The in-memory
history view retains only the newest entries, bounded to 1,000 records and 4 MiB of
serialized data. `GET /v1/pricing/audit` may therefore return fewer records than its
requested limit. The verify endpoint reports the total verified record count. This
does not detect deletion of a valid suffix that occurred before the first open, since
the file has no independent external head anchor.

## API walkthrough

```bash
TOKEN=...

# 1. define a rule: 20.00–40.00, anchored at 25.00, max step 1.00
curl -s -X PUT localhost:7070/v1/pricing/rules/skirt \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"item_id":"skirt","min_price_cents":2000,"max_price_cents":4000,
       "anchor_price_cents":2500,"step_cents":100}'

# 2. move the price five times
curl -s -X POST localhost:7070/v1/pricing/rules/skirt/tick \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"steps":5}'

# 3. quote an offer valid for one minute, then buy it
curl -s -X POST localhost:7070/v1/pricing/offers \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"item_id":"skirt","ttl_ms":60000}'
curl -s -X POST localhost:7070/v1/pricing/purchases \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"offer_id":"offer-…","quantity":2}'

# 4. try it again: 409 offer_already_redeemed
# 5. verify the audit chain
curl -s localhost:7070/v1/pricing/audit/verify -H "Authorization: Bearer $TOKEN"
```

## What is deliberately not here

* **No automatic background price movement.** Prices only change when a tick is
  requested, so a merchant can never be surprised by an unattended loop.
* **No negative discounts / no "surge" above `max_price_cents`.**
* **No purchase refunds or edits.** A refund is an application-level concept; the
  database only guarantees that the original record is never rewritten.
* **No currency conversion, taxes or rounding rules.** All amounts are integer cents.
* **No cross-item dependencies** (bundles, companions): that belongs to the shop catalog.

## Tests

`crates/preppydb-pricing` has 10 unit tests covering:

* rule validation rejects every invalid bound (including zero/negative prices);
* clamping on the normal path and after discounts;
* seeded determinism and seed fingerprints;
* step limits and floor enforcement with 90 % discounts;
* offers: expiry, staleness after a rule version bump, double redemption;
* purchase immutability at the ledger level;
* audit chain tamper detection.

`crates/preppydb-server/tests/api_integration.rs` adds HTTP-level tests for the same flow
(rule → tick → offer → purchase → stale offer → audit verify).
