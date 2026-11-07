-- End-to-end test of the Lua client against a running PreppyDB server.
--
--   PREPPYDB_URL=http://127.0.0.1:7077 \
--   PREPPYDB_TOKEN=<admin token> luvit clients/luvit/tests/test_live.lua
--
-- Under luvit the default `http` transport is used together with `options.wait`
-- (which drains the libuv loop). The Python runner in this directory injects a
-- synchronous transport instead, so the suite also runs where luvit is absent.
-- See clients/luvit/README.md for exactly what each path covers.

package.path = "./clients/luvit/?.lua;./clients/luvit/?/init.lua;" .. package.path

local loaded, preppydb = pcall(require, "../preppydb")
if not loaded then preppydb = require("preppydb") end
local harness = require("tests.harness")

local url = os.getenv("PREPPYDB_URL")
local token = os.getenv("PREPPYDB_TOKEN")
if not url or not token then
  io.write("skipping live tests: set PREPPYDB_URL and PREPPYDB_TOKEN\n")
  os.exit(0)
end

local t = harness.new("lua client (live server)")

-- A transport injected by the runner wins; otherwise use the luvit default.
local transport = _G.PREPPYDB_TRANSPORT
local wait
if not transport then
  local ok, uv = pcall(require, "uv")
  if ok then
    wait = function() uv.run() end
  end
end

local client, err = preppydb.new({
  url = url,
  token = token,
  transport = transport,
  wait = wait,
  timeout = 15000,
})
if not client then
  io.write("cannot create client: " .. tostring(err) .. "\n")
  os.exit(1)
end

local suffix = tostring(os.time()) .. "_" .. tostring(math.random(100000, 999999))
local collection = "lua_test_" .. suffix

-- ---------------------------------------------------------------- public API
local health = client:health()
t:truthy(health and health.status == "ok", "healthz answers ok")
local version = client:version()
t:equal(version.engine, "preppydb", "version identifies the engine")

-- ---------------------------------------------------------------- auth
local anonymous = select(1, preppydb.new({
  url = url,
  token = "definitely-not-a-token",
  transport = transport,
  wait = wait,
}))
local _, auth_error = anonymous:stats()
t:equal(auth_error and auth_error.status, 401, "a bad token is rejected with 401")
t:equal(auth_error and auth_error.code, "invalid_token", "401 carries a stable code")

local no_token = select(1, preppydb.new({ url = url, transport = transport, wait = wait }))
local _, missing_token = no_token:stats()
t:equal(missing_token and missing_token.code, "authentication_required", "missing token is reported separately")

-- ---------------------------------------------------------------- collections
local created, create_error = client:create_collection(collection, {
  type = "object",
  required = preppydb.array({ "name" }),
  properties = { name = { type = "string" } },
})
t:truthy(created, "collection is created: " .. tostring(create_error and create_error.message))
t:truthy(client:collection(collection), "collection is listed")
local names = client:collections()
local present = false
for _, name in ipairs(names or preppydb.array({})) do
  if name == collection then present = true end
end
t:truthy(present, "collections() contains the new collection")

-- ---------------------------------------------------------------- documents
local version1 = client:put(collection, "first", { name = "Molly" })
t:equal(version1, 1, "first write is version 1")
local value = client:get(collection, "first")
t:equal(value.name, "Molly", "document round-trips")
local version2 = client:put_checked(collection, "first", { name = "Molly v2" }, version1)
t:equal(version2, 2, "compare-and-set bumps the version")
local _, conflict = client:put_checked(collection, "first", { name = "stale" }, version1)
t:equal(conflict and conflict.code, "conflict", "stale compare-and-set is a conflict")
t:equal(conflict and conflict.retryable, false, "conflicts require caller reconciliation")

local _, validation = client:put(collection, "bad", { age = 3 })
t:equal(validation and validation.code, "validation_failed", "schema violations are rejected")
t:equal(client:get(collection, "bad"), nil, "rejected documents are not stored")

-- Unicode, slashes and spaces in keys and values survive the round-trip.
local tricky_key = "k/ä ö/" .. suffix
client:put(collection, tricky_key, { name = "Grüße 🌸", note = preppydb.null })
local tricky = client:get(collection, tricky_key)
t:equal(tricky.name, "Grüße 🌸", "unicode values round-trip")
t:equal(tricky.note, preppydb.null, "explicit null round-trips")

local docs = client:scan(collection, { limit = 10 })
t:truthy(#docs >= 2, "scan returns the stored documents")
t:equal(client:count(collection), #docs, "count matches the scan")

-- ---------------------------------------------------------------- batch
local batch, batch_error = client:batch(collection, {
  { op = "put", key = "b1", value = { name = "one" } },
  { op = "put", key = "b2", value = { name = "two" } },
})
t:truthy(batch, "batch succeeds: " .. tostring(batch_error and batch_error.message))
t:equal(batch and batch.operations, 2, "batch reports two operations")
local _, batch_conflict = client:batch(collection, {
  { op = "put", key = "b3", value = { name = "three" } },
  { op = "put", key = "b4", value = { name = "four" }, expected_version = 9 },
})
t:equal(batch_conflict and batch_conflict.code, "conflict", "a failing batch reports the conflict")
t:equal(client:get(collection, "b3"), nil, "a failing batch stores nothing")

-- ---------------------------------------------------------------- delete
t:equal(client:delete(collection, "b1"), true, "delete succeeds")
t:equal(client:get(collection, "b1"), nil, "deleted document is gone")

-- ---------------------------------------------------------------- pricing
local rule = client:put_price_rule({
  item_id = "lua_" .. suffix,
  min_price_cents = 1000,
  max_price_cents = 2000,
  anchor_price_cents = 1500,
  step_cents = 100,
})
t:truthy(rule and rule.version, "price rule is stored")
local tick = client:tick("lua_" .. suffix, 4)
t:equal(tick and #tick.changes, 4, "tick produced four changes")
for _, change in ipairs(tick and tick.changes or preppydb.array({})) do
  local cents = change.new_cents
  if type(cents) ~= "number" then cents = tonumber(change.new_cents) end
  t:truthy(cents >= 1000 and cents <= 2000, "price stays inside its bounds (" .. tostring(cents) .. ")")
end

local offer = client:offer("lua_" .. suffix, 60000)
t:truthy(offer and offer.offer_id, "offer is created")
local purchase, purchase_error = client:purchase(offer.offer_id, 1)
t:truthy(purchase and purchase.purchase, "purchase succeeds: " .. tostring(purchase_error and purchase_error.message))
local _, redeemed = client:purchase(offer.offer_id, 1)
t:equal(redeemed and redeemed.status, 409, "an offer cannot be redeemed twice")

local history = client:price_history("lua_" .. suffix)
t:equal(history and #history, 4, "price history holds every change")

local simulation = client:simulate({
  item_id = "sim_" .. suffix,
  min_price_cents = 100,
  max_price_cents = 200,
  anchor_price_cents = 150,
  step_cents = 20,
}, 6, "lua-seed")
t:equal(simulation and #simulation.history, 6, "simulation is seeded and returns six changes")

-- ---------------------------------------------------------------- admin
local verified = client:verify()
t:truthy(verified, "verify answers")
t:equal(verified and verified.ok, true, "storage verifies clean")

-- ---------------------------------------------------------------- cleanup
local dropped, drop_error = client:drop_collection(collection)
t:truthy(dropped, "collection is dropped: " .. tostring(drop_error and drop_error.message))
local _, missing = client:collection(collection)
t:equal(missing and missing.status, 404, "dropped collection is gone")

os.exit(t:report())
