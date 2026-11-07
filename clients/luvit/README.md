# PreppyDB Lua client

A dependency-free Lua client for the PreppyDB `/v1` HTTP API, written for
[Luvit](https://luvit.io) (LuaJIT) but compatible with Lua 5.1+; it ships its own JSON
codec, so nothing has to be installed next to it.

```lua
local preppydb = require("./preppydb") -- Luvit: relative to the calling file

local db, err = preppydb.new({
  url = "http://127.0.0.1:7077",
  token = os.getenv("PREPPYDB_TOKEN"),
})
if not db then error(err) end

local ok, health = db:health()
print(health.status)                       -- "ok"

local version = db:put("themes", "pastel", { name = "Pastel" })
local value = db:get("themes", "pastel")
print(value.name, version)                 -- Pastel  1

local updated, conflict = db:put_checked("themes", "pastel", { name = "Pastel 2" }, version)
print(updated, conflict)                   -- 2  nil
```

## Installation

Copy `preppydb.lua` next to your code (or onto your `package.path`):

```bash
cp preppydb.lua /path/to/app/deps/preppydb.lua
```

There is no `lit` package and no build step. Under luvit:

```lua
local preppydb = require("./preppydb") -- place preppydb.lua alongside the caller
```

The default response body limit is 16 MiB; configure `max_response_bytes` in
`preppydb.new` when a different limit is needed. The default transport stops
collecting chunks when that limit is exceeded. Custom transports must enforce
the same limit while receiving data: the client can check their returned body
before decoding but cannot prevent allocations inside a custom transport.

## API

Everything returns `result, err`:

* `result` — the unwrapped `data` payload of `/v1/*` API responses; `/healthz`,
  `/readyz`, and custom non-API routes return their flat bodies as-is;
* `err` — `{ status, code, message, retryable }`, or a transport error such as
  `{ code = "transport_error", message = "...", retryable = false }`.

Only `get` is special: a `404` means "no such document" and returns `nil, nil`.

### Client

| Method | Notes |
| --- | --- |
| `preppydb.new(opts)` | `url`, `token`, `prefix`, `timeout` (ms), `transport`, `retries`, `wait` |
| `db:health()`, `db:ready()`, `db:version()` | public endpoints |
| `db:login(user, password)` | stores the session token for later calls |
| `db:whoami()`, `db:stats()`, `db:metrics()` | |
| `db:collections()` | names of all collections |
| `db:collection(name)`, `db:create_collection(name, schema)`, `db:drop_collection(name)` | |
| `db:create_index(coll, name, field, unique)` | |
| `db:put(coll, key, value[, expected_version])`, `db:put_checked(coll, key, value, expected_version)` | returns the new version |
| `db:get(coll, key)`, `db:get_envelope(coll, key)` | value only vs. full envelope |
| `db:delete(coll, key[, expected_version])` | `true` on success |
| `db:scan(coll, { limit, prefix, offset, reverse })` | |
| `db:count(coll)` | document count from the collection info |
| `db:batch(coll, ops)` | `{ { op = "put", key = "k", value = {...}, expected_version = 0 }, ... }` |
| `db:find(coll, index, value)`, `db:range(coll, index, from, to)` | |
| `db:checkpoint()`, `db:compact()`, `db:verify()`, `db:backup(path)`, `db:recovery()` | admin |
| `db:bootstrap_personality()`, `db:personality_overview()`, `db:personality_profiles()` | |
| `db:price_rules()`, `db:put_price_rule(rule)`, `db:tick(item, steps)` | |
| `db:offer(item, ttl_ms)`, `db:purchase(offer_id, quantity)`, `db:price_history(item)` | |
| `db:simulate(rule, steps, seed)`, `db:audit_verify()` | |

Base URLs cannot include a query or fragment; `prefix` must be an origin-relative path.
The low-level `db:call` and `db:call_async` methods reject invalid HTTP methods and unsafe
request targets before invoking the transport.

### JSON helpers

* `preppydb.encode(value)` / `preppydb.decode(text)` — a self-contained codec with sorted
  object keys (stable payloads) and proper UTF-8 validation.
* `preppydb.array({...})` marks a table as a JSON array (needed for *empty* arrays);
  decoded arrays are marked automatically and `preppydb.is_array(t)` tells them apart.
* `preppydb.null` represents JSON `null`, because `nil` in a Lua table means "absent".

### Transports

The default transport uses luvit's `http` module. For advanced use (coroutine schedulers,
LuaSocket, tests) pass your own:

```lua
local db = preppydb.new({
  url = "http://127.0.0.1:7077",
  transport = function(request, callback)
    -- request = { method, url, headers, body, timeout_ms }
    my_http(request.method, request.url, request.headers, request.body, function(status, body)
      callback(status, body, {})
    end)
  end,
})
```

A synchronous transport calls `callback` before returning. An asynchronous one (like the
default) needs `wait` so the blocking methods can drain the event loop:

```lua
local db = preppydb.new({ url = "...", wait = function() require("uv").run() end })
```

For event-loop code prefer the non-blocking form:

```lua
db:call_async("GET", "/v1/stats", nil, function(result, err)
  if err then print("failed", err.code) else print(preppydb.encode(result)) end
end)
```

## Retries

`db:retries = N` (or `{ retries = N }`) accepts an integer from 0 to 10 and retries only
explicit `429` rate-limit responses. Per-call overrides use the same bound.
Transport errors and `5xx` responses are not retried automatically because the server may
have applied a write before the response was lost or failed. A `409` conflict also needs
caller reconciliation. Retry those cases only after checking the operation's outcome or
proving the operation is idempotent.

## Tests

```bash
# Offline suites (JSON codec + client logic with a fake transport) — any Lua works:
lua clients/luvit/tests/test_json.lua
lua clients/luvit/tests/test_client.lua

# Everything through an embedded Lua interpreter, no luvit required:
python3 -m pip install lupa
python3 clients/luvit/tests/run_with_lupa.py

# Live suite against a running server (needs a token):
PREPPYDB_URL=http://127.0.0.1:7077 PREPPYDB_TOKEN=... \
  luvit clients/luvit/tests/test_live.lua
```

`run_with_lupa.py --live` runs the same live suite but performs the HTTP requests through
Python's `http.client`, because luvit is not available in that environment. Everything
except the transport (URL building, JSON, headers, response handling, error mapping) is
the real Lua code; the luvit `http` transport itself is only exercised by the luvit run.
See [../../docs/TESTING.md](../../docs/TESTING.md) for the exact status.

## Limitations

* Plain HTTP only; terminate TLS in a proxy.
* No connection reuse (one request per connection, matching the server).
* No streaming for very large scans; page with `limit`/`offset`.
