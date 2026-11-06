-- Client tests against a recording fake transport. No network access required, so
-- this file runs on plain Lua, LuaJIT and luvit alike.
package.path = "./clients/luvit/?.lua;./clients/luvit/?/init.lua;" .. package.path

local preppydb = require("preppydb")
local harness = require("tests.harness")

local t = harness.new("lua client")

-- ---------------------------------------------------------------- fake server
local function recorder(responses)
  local calls = {}
  local transport = function(request, callback)
    calls[#calls + 1] = request
    local next_response = table.remove(responses, 1)
    if not next_response then
      error("fake transport ran out of responses after " .. #calls .. " request(s)")
    end
    if next_response.transport_error then
      callback(nil, next_response.transport_error, {})
      return
    end
    callback(next_response.status, next_response.body or "", next_response.headers or {})
  end
  return transport, calls
end

local function json_response(status, value)
  return { status = status, body = preppydb.encode(value) }
end

local function new_client(responses, options)
  local transport, calls = recorder(responses)
  options = options or {}
  options.url = options.url or "http://127.0.0.1:7077"
  options.token = options.token or "test-token"
  options.transport = transport
  local client, err = preppydb.new(options)
  assert(client, err)
  return client, calls
end

-- ------------------------------------------------------------- happy paths
do
  local client, calls = new_client({
    json_response(200, { status = "ok", checks = {} }),
    json_response(200, { engine = "preppydb", api = "v1" }),
    json_response(200, { data = { version = 3 } }),
  })
  local health, err = client:health()
  t:truthy(health and health.status == "ok", "health returns the parsed body")
  t:equal(err, nil, "health has no error")
  t:equal(calls[1].url, "http://127.0.0.1:7077/healthz", "health url")
  t:equal(calls[1].headers["Authorization"], "Bearer test-token", "token is sent")
  t:equal(calls[1].method, "GET", "health method")
  t:equal(calls[1].body, nil, "GET has no body")

  local version = client:version()
  t:equal(version.engine, "preppydb", "version parses")

  local written = client:put("themes", "pastel/2024", { name = "Pastel" })
  t:equal(written, 3, "put returns the version")
  t:equal(calls[3].method, "POST", "put method")
  t:equal(calls[3].url, "http://127.0.0.1:7077/v1/collections/themes/documents", "put url")
  t:equal(calls[3].headers["Content-Type"], "application/json", "put content type")
  t:equal(calls[3].body, '{"key":"pastel/2024","value":{"name":"Pastel"}}', "put body")
  t:equal(calls[3].headers["Content-Length"], tostring(#calls[3].body), "content length")
end

do
  -- Keys with slashes, spaces and unicode must be escaped in the path.
  local client, calls = new_client({
    json_response(200, { data = { version = 1 } }),
    json_response(200, { data = { key = "a b", value = 7 } }),
    json_response(200, { data = { deleted = "a b" } }),
  })
  client:put("coll", "a b/c", { v = 1 })
  t:equal(calls[1].url, "http://127.0.0.1:7077/v1/collections/coll/documents", "collection path is fixed")
  local envelope = client:get_envelope("coll", "a b/c")
  t:equal(
    calls[2].url,
    "http://127.0.0.1:7077/v1/collections/coll/documents/a%20b%2Fc",
    "document path escapes space and slash"
  )
  t:equal(envelope.value, 7, "get_envelope returns the value")
  t:equal(client:delete("coll", "a b/c", 4), true, "delete reports success")
  t:equal(
    calls[3].url,
    "http://127.0.0.1:7077/v1/collections/coll/documents/a%20b%2Fc?expected_version=4",
    "delete passes expected_version"
  )
end

do
  -- `get` turns a 404 into nil, everything else into an error.
  local client = new_client({ { status = 404, body = '{"error":{"code":"not_found","message":"no such key"}}' } })
  local value, err = client:get("coll", "missing")
  t:equal(value, nil, "missing document yields nil")
  t:equal(err, nil, "missing document is not an error")

  local other = new_client({ { status = 500, body = "boom" } })
  local _, failure = other:get("coll", "key")
  t:truthy(failure and failure.code == "http_error", "a 500 is surfaced as an error")
end

do
  local client, calls = new_client({
    json_response(201, { data = { collections = { { name = "b" }, { name = "a" } } } }),
    json_response(200, { data = { documents = { { key = "x", value = { n = 1 } } }, count = 1 } }),
    json_response(200, { data = { documents = { { key = "x" } } } }),
    json_response(200, { data = { documents = { { key = "x" } } } }),
    json_response(200, { data = { operations = 2 } }),
  })
  local names = client:collections()
  t:equal(names[1], "b", "collection names are extracted")
  t:equal(#names, 2, "all collections are returned")

  local docs = client:scan("c", { limit = 5, prefix = "x" })
  t:equal(docs[1].value.n, 1, "scan returns documents")
  t:equal(
    calls[2].url,
    "http://127.0.0.1:7077/v1/collections/c/documents?limit=5&prefix=x",
    "scan query string"
  )

  client:scan("c", { limit = 2, offset = 3, reverse = true })
  t:equal(
    calls[3].url,
    "http://127.0.0.1:7077/v1/collections/c/documents?limit=2&offset=3&reverse=true",
    "scan pagination and ordering"
  )

  local hits = client:find("c", "by_price", 50)
  t:equal(hits[1].key, "x", "index lookup returns documents")
  t:equal(
    calls[4].url,
    "http://127.0.0.1:7077/v1/collections/c/indexes/by_price?value=50",
    "index lookup query string"
  )

  local batch = client:batch("c", { { op = "put", key = "a", value = 1 }, { op = "delete", key = "b" } })
  t:equal(batch.operations, 2, "batch returns the operation count")
  t:equal(
    calls[5].body,
    '{"operations":[{"key":"a","op":"put","value":1},{"key":"b","op":"delete"}]}',
    "batch body is a JSON array of operations"
  )
end

-- ------------------------------------------------------------- error mapping
do
  local client = new_client({
    { status = 401, body = '{"error":{"code":"authentication_required","message":"token required"}}' },
  })
  local _, err = client:stats()
  t:equal(err.status, 401, "401 status is preserved")
  t:equal(err.code, "authentication_required", "error code is preserved")
  t:equal(err.message, "token required", "error message is preserved")
  t:equal(err.retryable, false, "auth errors are not retryable")
end

do
  local client = new_client({
    { status = 409, body = '{"error":{"code":"conflict","message":"version mismatch"}}' },
  })
  local _, err = client:put("c", "k", { a = 1 }, 7)
  t:equal(err.code, "conflict", "conflict code")
  t:equal(err.retryable, true, "conflicts are retryable")
  t:equal(err.status, 409, "conflict status")
end

do
  local client = new_client({ { status = 429, body = "" } })
  local _, err = client:stats()
  t:equal(err.code, "http_error", "empty bodies fall back to a generic code")
  t:equal(err.retryable, true, "429 is retryable")
end

do
  -- Non-JSON error pages must not raise.
  local client = new_client({ { status = 502, body = "<html>bad gateway</html>" } })
  local _, err = client:stats()
  t:equal(err.message, "<html>bad gateway</html>", "raw body is used as the message")
end

do
  -- Retries: two failures then success.
  local client, calls = new_client({
    { status = 503, body = "unavailable" },
    { status = 503, body = "unavailable" },
    json_response(200, { data = { ok = true } }),
  }, { retries = 2 })
  local stats = client:stats()
  t:equal(stats.ok, true, "retry eventually succeeds")
  t:equal(#calls, 3, "retry used exactly three attempts")

  local no_retry, limited = new_client({
    { status = 401, body = '{"error":{"code":"invalid_token","message":"nope"}}' },
  }, { retries = 5 })
  local _, err = no_retry:stats()
  t:equal(#limited, 1, "non-retryable errors are not repeated")
  t:equal(err.code, "invalid_token", "non-retryable error code")
end

do
  -- Transport failures are reported as transport_error and are retryable.
  local client = new_client({ { transport_error = "connection refused" } }, { retries = 0 })
  local _, err = client:stats()
  t:equal(err.code, "transport_error", "transport failures have their own code")
  t:equal(err.retryable, true, "transport failures are retryable")
end

do
  -- Malformed success bodies are rejected instead of silently returning garbage.
  local client = new_client({ { status = 200, body = "{oops" } })
  local _, err = client:stats()
  t:equal(err.code, "invalid_response", "malformed JSON is an invalid_response error")
end

do
  -- An asynchronous transport without a wait hook is caught, not silently hung.
  local pending = preppydb.new({
    url = "http://127.0.0.1:7077",
    token = "t",
    transport = function() end,
  })
  local _, err = pending:stats()
  t:equal(err.code, "transport_incomplete", "incomplete async transports are reported")
end

-- ---------------------------------------------------------------- validation
do
  local client, calls = new_client({ json_response(200, { data = { ok = 1 } }) })
  local _, err = client:put_price_rule({ min_price_cents = 100 })
  t:equal(err.code, "invalid_argument", "a rule without item_id is rejected locally")
  t:equal(#calls, 0, "invalid input never reaches the network")

  local ok, failure = preppydb.new({ url = "https://example.org" })
  t:equal(ok, nil, "https urls are refused")
  t:truthy(failure:find("plain HTTP"), "the https refusal explains itself")

  local good, calls2 = new_client({
    json_response(200, { data = { version = 2 } }),
    json_response(200, { data = { offer_id = "of_1", price_cents = 2500 } }),
    json_response(200, { data = { purchase = { price_cents = 2500 } } }),
  }, {})
  local rule = good:put_price_rule({ item_id = "skirt", min_price_cents = 2000, max_price_cents = 4000 })
  t:equal(rule.version, 2, "price rule is written")
  t:equal(calls2[1].method, "PUT", "rule uses PUT")
  t:equal(calls2[1].url, "http://127.0.0.1:7077/v1/pricing/rules/skirt", "rule url")
  local offer = good:offer("skirt", 60000)
  t:equal(offer.offer_id, "of_1", "offer id is returned")
  t:equal(calls2[2].body, '{"item_id":"skirt","ttl_ms":60000}', "offer body")
  local purchase = good:purchase("of_1", 2)
  t:equal(purchase.purchase.price_cents, 2500, "purchase price is returned")
  t:equal(calls2[3].body, '{"offer_id":"of_1","quantity":2}', "purchase body")
end

do
  -- A login sets the token for later requests.
  local client, calls = new_client({
    json_response(200, { data = { token = "session-abc", expires_at = 1 } }),
    json_response(200, { data = { username = "admin" } }),
  }, { token = nil })
  t:equal(calls[1] and calls[1].headers["Authorization"], nil, "no token before login")
  local outcome = client:login("admin", "correct horse battery")
  t:equal(outcome.token, "session-abc", "login returns a session token")
  client:whoami()
  t:equal(calls[2].headers["Authorization"], "Bearer session-abc", "session token is reused")
  t:equal(
    calls[1].body,
    '{"password":"correct horse battery","username":"admin"}',
    "login body carries the credentials"
  )
end

do
  -- Verify maps the 409-with-findings case to ok=false instead of an error.
  local client = new_client({
    { status = 409, body = '{"error":{"code":"conflict","message":"3 issues"}}' },
    json_response(200, { data = { ok = true, issues = preppydb.array({}) } }),
  })
  local bad = client:verify()
  t:equal(bad.ok, false, "verify reports issues without throwing")
  local fine = client:verify()
  t:equal(fine.ok, true, "verify reports success")
end

do
  -- A prefix in the url is preserved.
  local client, calls = new_client({ json_response(200, { status = "ok" }) }, { url = "http://db.local:8080/api" })
  client:health()
  t:equal(calls[1].url, "http://db.local:8080/api/healthz", "prefix is kept")
end

do
  local client = new_client({ { status = 200, body = '{"large":true}' } }, { max_response_bytes = 4 })
  local _, err = client:health()
  t:equal(err.code, "invalid_response", "custom transport response is checked before decoding")
  local async_client = new_client({ { status = 200, body = '{"large":true}' } }, { max_response_bytes = 4 })
  async_client:call_async("GET", "/healthz", nil, function(_, async_err)
    t:equal(async_err.code, "invalid_response", "async response limit")
  end)
end

do
  local old_http = package.loaded.http
  local destroyed = 0
  package.loaded.http = {
    parseUrl = function() return {} end,
    request = function(_, on_response)
      local handlers = {}
      local response = {
        statusCode = 200, headers = {},
        on = function(_, event, callback) handlers[event] = callback end,
        destroy = function() destroyed = destroyed + 1 end,
      }
      return {
        on = function(self) return self end,
        setTimeout = function() end,
        done = function()
          on_response(response)
          handlers.data("123")
          handlers.data("456")
          handlers["end"]()
        end,
      }
    end,
  }
  local client = assert(preppydb.new({ url = "http://localhost:7077", max_response_bytes = 4 }))
  local _, err = client:health()
  t:equal(err.code, "transport_error", "default transport stops oversized stream")
  t:equal(destroyed, 1, "oversized stream destroyed once")
  package.loaded.http = old_http
end

os.exit(t:report())
