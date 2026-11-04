-- Minimal test harness (no dependencies, works on Lua 5.1+ / LuaJIT / luvit).
--
--   local t = require("tests.harness").new("json")
--   t:equal(1 + 1, 2, "addition")
--   os.exit(t:report())
local harness = {}

local Harness = {}
Harness.__index = Harness

function harness.new(name)
  return setmetatable({ name = name, passed = 0, failed = 0, failures = {} }, Harness)
end

function Harness:record(ok, description, detail)
  if ok then
    self.passed = self.passed + 1
    io.write("  ok   ", description, "\n")
  else
    self.failed = self.failed + 1
    self.failures[#self.failures + 1] = description .. (detail and (" :: " .. detail) or "")
    io.write("  FAIL ", description, detail and (" :: " .. detail) or "", "\n")
  end
  return ok
end

local function show(value)
  if type(value) == "table" then
    local ok, encoded = pcall(function() return require("preppydb").encode(value) end)
    if ok then return encoded end
    return "<table>"
  end
  return tostring(value)
end

function Harness:equal(actual, expected, description)
  local ok = actual == expected
  return self:record(
    ok,
    description,
    ok and nil or ("expected " .. show(expected) .. ", got " .. show(actual))
  )
end

function Harness:truthy(value, description)
  return self:record(value and true or false, description, value and nil or "value was falsy")
end

function Harness:falsy(value, description)
  return self:record(not value, description, value and ("unexpected " .. show(value)) or nil)
end

function Harness:error_with(fn, code, description)
  local ok, err = fn()
  return self:record(
    ok == nil and type(err) == "table" and err.code == code,
    description,
    "got " .. show(ok or err) .. (type(err) == "table" and (" code=" .. tostring(err.code)) or "")
  )
end

function Harness:report()
  io.write(string.format("\n%s: %d passed, %d failed\n", self.name, self.passed, self.failed))
  if self.failed > 0 then
    for _, failure in ipairs(self.failures) do
      io.write("  - ", failure, "\n")
    end
    return 1
  end
  return 0
end

return harness
