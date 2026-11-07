-- Unit tests for the bundled JSON codec.
package.path = "./clients/luvit/?.lua;./clients/luvit/?/init.lua;" .. package.path

local preppydb = require("preppydb")
local harness = require("tests.harness")

local t = harness.new("json codec")

t:equal(preppydb.encode({ a = 1 }), '{"a":1}', "encodes an object")
t:equal(preppydb.encode(preppydb.array({})), "[]", "empty array stays an array")
t:equal(preppydb.encode({}), "{}", "empty table is an object")
t:equal(preppydb.encode(preppydb.array({ 1, 2, 3 })), "[1,2,3]", "encodes an array")
t:equal(preppydb.encode({ b = true, a = preppydb.null }), '{"a":null,"b":true}', "sorted keys, null")
t:equal(preppydb.encode("a\"b\\c\nd"), '"a\\"b\\\\c\\nd"', "escapes control characters")
t:equal(preppydb.encode(1.5), "1.5", "encodes fractions")
t:equal(preppydb.encode(-0.25), "-0.25", "encodes negative fractions")
t:equal(preppydb.encode(2 ^ 40), "1099511627776", "large integers stay integral")
t:equal(preppydb.encode({ nested = { list = preppydb.array({ { 1 } }) } }), '{"nested":{"list":[[1]]}}', "nested")

local sample = '{"a":[1,2,{"b":null,"c":true}],"d":"x\\u00e9y","e":-2.5e3,"f":""}'
local decoded = preppydb.decode(sample)
t:equal(decoded.a[1], 1, "decodes array scalars")
t:equal(decoded.a[3].b, preppydb.null, "decodes null")
t:equal(decoded.a[3].c, true, "decodes true")
t:equal(decoded.d, "x\195\169y", "decodes \\u escapes to UTF-8")
t:equal(decoded.e, -2500, "decodes exponents")
t:equal(decoded.f, "", "decodes empty strings")
t:equal(preppydb.is_array(decoded.a), true, "decoded arrays are marked as arrays")
t:equal(preppydb.encode(decoded), preppydb.encode(preppydb.decode(sample)), "round-trips")

-- Surrogate pairs.
t:equal(preppydb.decode('"\\ud83d\\ude00"'), "\240\159\152\128", "combines surrogate pairs")

-- Error handling.
local ok, err = pcall(preppydb.decode, "{")
t:falsy(ok, "rejects truncated input")
t:truthy(tostring(err):find("invalid JSON"), "error mentions the format")
ok = pcall(preppydb.encode, { [1.5] = true })
t:falsy(ok, "rejects non-string object keys")
ok = pcall(preppydb.encode, { bad = "\255\254" })
t:falsy(ok, "rejects invalid UTF-8")

-- URL helpers.
t:equal(preppydb.encode_path("a/b c"), "a%2Fb%20c", "path encoding escapes slashes")
t:equal(preppydb.percent_encode('{"x":1}'), "%7B%22x%22%3A1%7D", "query encoding")

local parsed = preppydb.parse_url("http://127.0.0.1:7077/base")
t:equal(parsed.host, "127.0.0.1", "parses host")
t:equal(parsed.port, 7077, "parses port")
t:equal(parsed.prefix, "/base", "parses prefix")
t:equal(preppydb.parse_url("https://example.org"), nil, "rejects https")
local defaulted = preppydb.parse_url("http://example.org")
t:equal(defaulted.port, 80, "defaults to port 80")

os.exit(t:report())
