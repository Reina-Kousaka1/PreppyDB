--[[----------------------------------------------------------------------------
  PreppyDB Lua client — Lua 5.1+ / LuaJIT / Luvit.

  Self-contained: ships its own JSON codec and needs no third-party modules.
  The only platform-specific piece is the transport; the default one uses the
  `http` module that luvit provides. Anything else (a test double, a coroutine
  scheduler, LuaSocket) can be injected with `options.transport`.

  Usage (luvit):

      local preppydb = require("preppydb")
      local db = preppydb.new({ url = "http://127.0.0.1:7077", token = os.getenv("PREPPYDB_TOKEN") })
      local ok, health = db:health()
      local version = db:put("themes", "pastel", { name = "Pastel" })

  Every method returns `result` on success and `nil, err` on failure, where
  `err = { status = 409, code = "conflict", message = "...", retryable = true }`.
  Nothing raises for API errors, so callers can pattern-match on `err.code`.

  API version: /v1.  Requires a bearer token for everything except /healthz,
  /readyz and /v1/version.
------------------------------------------------------------------------------]]

local preppydb = {}

preppydb.VERSION = "0.1.0"
preppydb.API_VERSION = "v1"

--------------------------------------------------------------------- JSON codec

-- Marker for JSON `null`; `nil` cannot represent it because in Lua a missing key
-- and a null value are indistinguishable.
preppydb.null = setmetatable({}, { __tostring = function() return "null" end })

local array_mt = { __jsontype = "array" }

--- Mark a table as a JSON array (needed for empty arrays).
function preppydb.array(t)
  return setmetatable(t or {}, array_mt)
end

function preppydb.is_array(t)
  return getmetatable(t) == array_mt
end

local function is_utf8_safe(bytes)
  local i, n = 1, #bytes
  while i <= n do
    local c = bytes:byte(i)
    if c < 0x80 then
      i = i + 1
    elseif c >= 0xC2 and c <= 0xDF and i + 1 <= n and bytes:byte(i + 1) >= 0x80 then
      i = i + 2
    elseif c >= 0xE0 and c <= 0xEF and i + 2 <= n
      and bytes:byte(i + 1) >= 0x80 and bytes:byte(i + 2) >= 0x80 then
      i = i + 3
    elseif c >= 0xF0 and c <= 0xF4 and i + 3 <= n
      and bytes:byte(i + 1) >= 0x80 and bytes:byte(i + 2) >= 0x80 and bytes:byte(i + 3) >= 0x80 then
      i = i + 4
    else
      return false
    end
  end
  return true
end
preppydb.is_utf8 = is_utf8_safe

local escapes = {
  ['"'] = '\\"', ["\\"] = "\\\\", ["\b"] = "\\b", ["\f"] = "\\f",
  ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t",
}

local function encode_string(value)
  if not is_utf8_safe(value) then
    error("cannot encode a string that is not valid UTF-8", 3)
  end
  return '"' .. value:gsub('[%z\1-\31\\"]', function(char)
    return escapes[char] or string.format("\\u%04x", char:byte())
  end) .. '"'
end

local function number_to_string(value)
  if value ~= value then
    error("cannot encode NaN", 3)
  end
  if value == math.huge or value == -math.huge then
    error("cannot encode infinity", 3)
  end
  if value % 1 == 0 and math.abs(value) < 2 ^ 53 then
    return string.format("%d", value)
  end
  -- %.17g round-trips every IEEE-754 double; trim the trailing noise so that
  -- 0.5 does not become 0.50000000000000000.
  local text = string.format("%.17g", value)
  if tonumber(text) ~= value then
    error("number cannot be represented", 3)
  end
  return text
end

local encode_value

local function is_array_like(t)
  if getmetatable(t) == array_mt then
    return true
  end
  local count = 0
  local max = 0
  for key in pairs(t) do
    if type(key) ~= "number" or key % 1 ~= 0 or key < 1 then
      return false
    end
    count = count + 1
    if key > max then max = key end
  end
  return count > 0 and count == max
end

local function encode_table(t, seen)
  if seen[t] then
    error("cannot encode a cyclic table", 3)
  end
  seen[t] = true
  local out
  if is_array_like(t) then
    local parts = {}
    for i = 1, #t do
      parts[i] = encode_value(t[i], seen)
    end
    out = "[" .. table.concat(parts, ",") .. "]"
  else
    -- Deterministic key order makes payloads stable and testable.
    local keys = {}
    for key in pairs(t) do
      if type(key) ~= "string" then
        error("object keys must be strings, got " .. type(key), 3)
      end
      keys[#keys + 1] = key
    end
    table.sort(keys)
    local parts = {}
    for i = 1, #keys do
      parts[i] = encode_string(keys[i]) .. ":" .. encode_value(t[keys[i]], seen)
    end
    out = "{" .. table.concat(parts, ",") .. "}"
  end
  seen[t] = nil
  return out
end

encode_value = function(value, seen)
  local kind = type(value)
  if value == nil or value == preppydb.null then
    return "null"
  elseif kind == "boolean" then
    return value and "true" or "false"
  elseif kind == "number" then
    return number_to_string(value)
  elseif kind == "string" then
    return encode_string(value)
  elseif kind == "table" then
    return encode_table(value, seen or {})
  end
  error("cannot encode a " .. kind, 3)
end

function preppydb.encode(value)
  return encode_value(value, {})
end

local function skip_whitespace(text, index)
  while index <= #text do
    local char = text:sub(index, index)
    if char ~= " " and char ~= "\t" and char ~= "\n" and char ~= "\r" then
      return index
    end
    index = index + 1
  end
  return index
end

local function decode_error(text, index, message)
  local line = 1
  for _ in text:sub(1, index - 1):gmatch("\n") do
    line = line + 1
  end
  error(string.format("invalid JSON at byte %d (line %d): %s", index, line, message), 0)
end

local decode_value

local function decode_string(text, index)
  local out = {}
  index = index + 1
  while true do
    local char = text:sub(index, index)
    if char == "" then
      decode_error(text, index, "unterminated string")
    elseif char == '"' then
      return table.concat(out), index + 1
    elseif char == "\\" then
      local escape = text:sub(index + 1, index + 1)
      index = index + 2
      if escape == '"' or escape == "\\" or escape == "/" then
        out[#out + 1] = escape
      elseif escape == "b" then
        out[#out + 1] = "\b"
      elseif escape == "f" then
        out[#out + 1] = "\f"
      elseif escape == "n" then
        out[#out + 1] = "\n"
      elseif escape == "r" then
        out[#out + 1] = "\r"
      elseif escape == "t" then
        out[#out + 1] = "\t"
      elseif escape == "u" then
        local hex = text:sub(index, index + 3)
        if not hex:match("^%x%x%x%x$") then
          decode_error(text, index, "bad \\u escape")
        end
        local code = tonumber(hex, 16)
        index = index + 4
        -- Combine a UTF-16 surrogate pair written as two \u escapes.
        if code >= 0xD800 and code <= 0xDBFF and text:sub(index, index + 1) == "\\u" then
          local low_hex = text:sub(index + 2, index + 5)
          local low = low_hex:match("^%x%x%x%x$") and tonumber(low_hex, 16)
          if low and low >= 0xDC00 and low <= 0xDFFF then
            code = 0x10000 + (code - 0xD800) * 0x400 + (low - 0xDC00)
            index = index + 6
          end
        end
        if code < 0x80 then
          out[#out + 1] = string.char(code)
        elseif code < 0x800 then
          out[#out + 1] = string.char(0xC0 + math.floor(code / 0x40), 0x80 + code % 0x40)
        elseif code < 0x10000 then
          out[#out + 1] = string.char(
            0xE0 + math.floor(code / 0x1000),
            0x80 + math.floor(code / 0x40) % 0x40,
            0x80 + code % 0x40
          )
        else
          out[#out + 1] = string.char(
            0xF0 + math.floor(code / 0x40000),
            0x80 + math.floor(code / 0x1000) % 0x40,
            0x80 + math.floor(code / 0x40) % 0x40,
            0x80 + code % 0x40
          )
        end
      else
        decode_error(text, index - 1, "unknown escape \\" .. escape)
      end
    else
      out[#out + 1] = char
      index = index + 1
    end
  end
end

local function decode_number(text, index)
  local start = index
  if text:sub(index, index) == "-" then index = index + 1 end
  local digits = 0
  while text:sub(index, index):match("%d") do
    index = index + 1
    digits = digits + 1
  end
  if digits == 0 then
    decode_error(text, start, "expected a digit")
  end
  if text:sub(index, index) == "." then
    index = index + 1
    local fraction = 0
    while text:sub(index, index):match("%d") do
      index = index + 1
      fraction = fraction + 1
    end
    if fraction == 0 then decode_error(text, index, "expected a digit after '.'") end
  end
  local exponent = text:sub(index, index)
  if exponent == "e" or exponent == "E" then
    index = index + 1
    local sign = text:sub(index, index)
    if sign == "+" or sign == "-" then index = index + 1 end
    local digits_e = 0
    while text:sub(index, index):match("%d") do
      index = index + 1
      digits_e = digits_e + 1
    end
    if digits_e == 0 then decode_error(text, index, "expected exponent digits") end
  end
  return tonumber(text:sub(start, index - 1)), index
end

local function decode_array(text, index)
  local out = preppydb.array({})
  index = skip_whitespace(text, index + 1)
  if text:sub(index, index) == "]" then
    return out, index + 1
  end
  while true do
    local value
    value, index = decode_value(text, index)
    out[#out + 1] = value
    index = skip_whitespace(text, index)
    local char = text:sub(index, index)
    if char == "," then
      index = skip_whitespace(text, index + 1)
    elseif char == "]" then
      return out, index + 1
    else
      decode_error(text, index, "expected ',' or ']'")
    end
  end
end

local function decode_object(text, index)
  local out = {}
  index = skip_whitespace(text, index + 1)
  if text:sub(index, index) == "}" then
    return out, index + 1
  end
  while true do
    index = skip_whitespace(text, index)
    if text:sub(index, index) ~= '"' then
      decode_error(text, index, "expected a string key")
    end
    local key
    key, index = decode_string(text, index)
    index = skip_whitespace(text, index)
    if text:sub(index, index) ~= ":" then
      decode_error(text, index, "expected ':'")
    end
    index = skip_whitespace(text, index + 1)
    local value
    value, index = decode_value(text, index)
    out[key] = value
    index = skip_whitespace(text, index)
    local char = text:sub(index, index)
    if char == "," then
      index = skip_whitespace(text, index + 1)
    elseif char == "}" then
      return out, index + 1
    else
      decode_error(text, index, "expected ',' or '}'")
    end
  end
end

decode_value = function(text, index)
  index = skip_whitespace(text, index)
  local char = text:sub(index, index)
  if char == "" then
    decode_error(text, index, "unexpected end of input")
  elseif char == "{" then
    return decode_object(text, index)
  elseif char == "[" then
    return decode_array(text, index)
  elseif char == '"' then
    return decode_string(text, index)
  elseif char == "t" and text:sub(index, index + 3) == "true" then
    return true, index + 4
  elseif char == "f" and text:sub(index, index + 4) == "false" then
    return false, index + 5
  elseif char == "n" and text:sub(index, index + 3) == "null" then
    return preppydb.null, index + 4
  else
    return decode_number(text, index)
  end
end

function preppydb.decode(text)
  if type(text) ~= "string" then
    error("decode expects a string", 2)
  end
  local value, index = decode_value(text, 1)
  index = skip_whitespace(text, index)
  if index <= #text then
    decode_error(text, index, "trailing garbage")
  end
  return value
end

-------------------------------------------------------------------- HTTP helpers

local function percent_encode(value)
  return (tostring(value):gsub("[^%w%-%._~]", function(char)
    return string.format("%%%02X", char:byte())
  end))
end

local function encode_path(value)
  -- Slash and percent must be escaped so that keys may contain them; everything
  -- else that is not unreserved gets percent-encoded as well.
  return (tostring(value):gsub("[^%w%-%._~]", function(char)
    return string.format("%%%02X", char:byte())
  end))
end

preppydb.percent_encode = percent_encode
preppydb.encode_path = encode_path

local function parse_url(url)
  local scheme, rest = tostring(url):match("^(%w+)://(.*)$")
  if not scheme then
    return nil, "url must start with http:// (got '" .. tostring(url) .. "')"
  end
  if scheme ~= "http" then
    return nil, "unsupported scheme '" .. scheme .. "': the client speaks plain HTTP "
      .. "(terminate TLS in front of the server)"
  end
  local authority, path = rest:match("^([^/]*)(/.*)$")
  if not authority then
    authority, path = rest, ""
  end
  if path == "/" then path = "" end
  local host, port = authority:match("^(.+):(%d+)$")
  if not host then
    host, port = authority, "80"
  end
  if host == "" then
    return nil, "url has no host"
  end
  return { host = host, port = tonumber(port), prefix = path }
end

preppydb.parse_url = parse_url

--- Default transport: luvit's `http` module.
local function luvit_transport()
  local ok, http = pcall(require, "http")
  if not ok then
    return nil, "the luvit 'http' module is not available; pass options.transport "
      .. "(see clients/luvit/README.md): " .. tostring(http)
  end
  return function(request, callback)
    local finished = false
    local function finish(status, body, headers)
      if finished then return end
      finished = true
      callback(status, body, headers or {})
    end
    -- request = { method, url, headers, body, timeout }
    local http_options = http.parseUrl(request.url)
    http_options.method = request.method
    http_options.headers = request.headers
    local outgoing = http.request(http_options, function(response)
      local status = tonumber(response.statusCode) or 0
      local chunks = {}
      local size = 0
      local limit = request.max_response_bytes
      local headers = response.headers or {}
      local declared = headers["content-length"] or headers["Content-Length"]
      local function reject(message)
        chunks = {}
        finish(nil, message, {})
        if type(response.destroy) == "function" then pcall(response.destroy, response) end
      end
      if declared ~= nil and (type(declared) ~= "string" or not declared:match("^%d+$") or not tonumber(declared) or tonumber(declared) > limit) then
        reject("invalid or oversized response Content-Length")
        return
      end
      response:on("data", function(chunk)
        if finished then return end
        size = size + #chunk
        if size > limit then reject("response body exceeds configured limit"); return end
        chunks[#chunks + 1] = chunk
      end)
      response:on("end", function()
        if declared and size ~= tonumber(declared) then reject("response Content-Length mismatch"); return end
        finish(status, table.concat(chunks), headers)
      end)
      response:on("error", function(err) reject("response stream error: " .. tostring(err)) end)
    end)
    outgoing:on("error", function(err)
      finish(nil, tostring(err), {})
    end)
    if request.timeout_ms then
      outgoing:setTimeout(request.timeout_ms, function()
        finish(nil, "request timed out", {})
        outgoing:destroy()
      end)
    end
    outgoing:done(request.body)
    return nil
  end
end

-------------------------------------------------------------------------- Client

local Client = {}
Client.__index = Client

--- Create a client.
-- options.url          base url, e.g. "http://127.0.0.1:7077"
-- options.token        bearer token (optional; required for non-public endpoints)
-- options.prefix       extra path prefix, e.g. "/db"
-- options.timeout      milliseconds the transport may take (default 10000)
-- options.transport    function(request, callback); see README
-- options.retries      automatic retries on retryable errors (default 0)
function preppydb.new(options)
  options = options or {}
  if type(options) ~= "table" then
    error("preppydb.new expects an options table", 2)
  end
  local response_limit = options.max_response_bytes or 16 * 1024 * 1024
  if type(response_limit) ~= "number" or response_limit < 0 or response_limit ~= math.floor(response_limit) or response_limit > 2147483647 then
    return nil, "max_response_bytes must be an integer between 0 and 2147483647"
  end
  local parsed, err = parse_url(options.url or "http://127.0.0.1:7077")
  if not parsed then
    return nil, err
  end
  local transport, transport_err = options.transport
  if not transport then
    transport, transport_err = luvit_transport()
    if not transport then
      return nil, transport_err
    end
  end
  local self = setmetatable({
    host = parsed.host,
    port = parsed.port,
    prefix = (options.prefix or "") .. parsed.prefix,
    token = options.token,
    timeout_ms = options.timeout or 10000,
    max_response_bytes = response_limit,
    retries = options.retries or 0,
    wait = options.wait,
    transport = transport,
  }, Client)
  return self
end

function Client:request_url(path)
  return string.format("http://%s:%d%s%s", self.host, self.port, self.prefix, path)
end

local function error_from(status, body)
  local code, message = "http_error", body
  local ok, parsed = pcall(preppydb.decode, body)
  if ok and type(parsed) == "table" and type(parsed.error) == "table" then
    code = parsed.error.code or code
    message = parsed.error.message or message
  end
  -- 408, 409, 425 and 429 are safe to retry: nothing was half-applied.
  local retryable = status == 408 or status == 409 or status == 425 or status == 429
    or (status and status >= 500)
  return { status = status, code = code, message = message, retryable = retryable }
end

--- Issue one request synchronously.
-- Returns decoded body, or nil plus an error table.
function Client:call(method, path, body, options)
  local payload
  if body ~= nil then
    local ok, encoded = pcall(preppydb.encode, body)
    if not ok then
      return nil, { code = "encoding_error", message = tostring(encoded), retryable = false }
    end
    payload = encoded
  end
  local headers = { ["Accept"] = "application/json" }
  if payload then
    headers["Content-Type"] = "application/json"
    headers["Content-Length"] = tostring(#payload)
  end
  if self.token then
    headers["Authorization"] = "Bearer " .. self.token
  end
  local request = {
    method = method,
    url = self:request_url(path),
    headers = headers,
    body = payload,
    timeout_ms = (options and options.timeout) or self.timeout_ms,
    max_response_bytes = self.max_response_bytes,
  }
  local attempts = 1 + (options and options.retries or self.retries or 0)
  local last_error
  for attempt = 1, attempts do
    if options and options.on_request then
      options.on_request(attempt)
    end
    local finished = false
    local status, response_body, response_headers
    self.transport(request, function(result_status, result_body, result_headers)
      status = result_status
      response_body = result_body or ""
      response_headers = result_headers or {}
      finished = true
    end)
    -- A synchronous transport may already have called the callback; wait for the
    -- asynchronous ones by draining the loop, if the platform offers one.
    local wait = (options and options.wait) or self.wait
    if not finished and wait then
      while not finished do
        wait()
      end
    end
    if not finished then
      return nil, {
        code = "transport_incomplete",
        message = "the transport did not deliver a response; pass options.wait "
          .. "for asynchronous transports",
        retryable = true,
      }
    end
    if status ~= nil and (type(response_body) ~= "string" or #response_body > self.max_response_bytes) then
      return nil, { code = "invalid_response", message = "response body exceeds configured limit", retryable = false }
    end
    if status == nil then
      last_error = { code = "transport_error", message = tostring(response_body), retryable = true }
    elseif status >= 200 and status < 300 then
      if response_body == "" then
        return {}, nil
      end
      local ok, decoded = pcall(preppydb.decode, response_body)
      if not ok then
        return nil, {
          code = "invalid_response",
          message = "server returned invalid JSON: " .. tostring(decoded),
          status = status,
          retryable = false,
        }
      end
      -- Data endpoints wrap their payload in {"data": ...}; hand callers the payload
      -- so they do not have to know about the envelope. Public endpoints (/healthz,
      -- /v1/version) answer flat and are returned as-is.
      if type(decoded) == "table" and type(decoded.data) == "table" then
        return decoded.data, nil
      end
      return decoded, nil
    else
      last_error = error_from(status, response_body)
      last_error.headers = response_headers
    end
    if not last_error.retryable or attempt == attempts then
      break
    end
  end
  return nil, last_error
end

--- Asynchronous variant for event-loop code (luvit).
-- `callback(result, err)` receives what the synchronous methods return.
function Client:call_async(method, path, body, callback)
  local payload
  if body ~= nil then
    local encoded_ok, encoded = pcall(preppydb.encode, body)
    if not encoded_ok then
      callback(nil, { code = "encoding_error", message = tostring(encoded), retryable = false })
      return
    end
    payload = encoded
  end
  local headers = { ["Accept"] = "application/json" }
  if self.token then
    headers["Authorization"] = "Bearer " .. self.token
  end
  if payload then
    headers["Content-Type"] = "application/json"
    headers["Content-Length"] = tostring(#payload)
  end
  self.transport({
    method = method,
    url = self:request_url(path),
    headers = headers,
    body = payload,
    timeout_ms = self.timeout_ms,
    max_response_bytes = self.max_response_bytes,
  }, function(status, response_body, response_headers)
    if status == nil then
      callback(nil, { code = "transport_error", message = tostring(response_body), retryable = true })
      return
    end
    if type(response_body or "") ~= "string" or #(response_body or "") > self.max_response_bytes then
      callback(nil, { code = "invalid_response", message = "response body exceeds configured limit", retryable = false })
      return
    end
    if status >= 200 and status < 300 then
      local decoded_ok, decoded = pcall(preppydb.decode, response_body or "")
      if not decoded_ok then
        callback(nil, {
          code = "invalid_response",
          message = "server returned invalid JSON: " .. tostring(decoded),
          status = status,
          retryable = false,
        })
        return
      end
      if type(decoded) == "table" and type(decoded.data) == "table" then
        callback(decoded.data, nil)
      else
        callback(decoded, nil)
      end
      return
    end
    local err = error_from(status, response_body or "")
    err.headers = response_headers
    callback(nil, err)
  end)
end

----------------------------------------------------------------------- Endpoints

function Client:health()
  return self:call("GET", "/healthz")
end

function Client:ready()
  return self:call("GET", "/readyz")
end

function Client:version()
  return self:call("GET", "/v1/version")
end

function Client:login(username, password)
  local data, err = self:call("POST", "/v1/auth/login", { username = username, password = password })
  if data and data.token then
    self.token = data.token
  end
  return data, err
end

function Client:whoami()
  return self:call("GET", "/v1/auth/whoami")
end

function Client:stats()
  return self:call("GET", "/v1/stats")
end

function Client:metrics()
  return self:call("GET", "/v1/metrics")
end

function Client:collections()
  local data, err = self:call("GET", "/v1/collections")
  if not data then return nil, err end
  local names = preppydb.array({})
  for _, entry in ipairs(data.collections or {}) do
    names[#names + 1] = type(entry) == "table" and entry.name or entry
  end
  return names
end

function Client:collection(name)
  return self:call("GET", "/v1/collections/" .. encode_path(name))
end

function Client:create_collection(name, schema)
  local body = { name = name }
  if schema ~= nil then body.schema = schema end
  return self:call("POST", "/v1/collections", body)
end

function Client:drop_collection(name)
  return self:call("DELETE", "/v1/collections/" .. encode_path(name) .. "?confirm=true")
end

function Client:create_index(collection, name, field, unique)
  return self:call("POST", "/v1/collections/" .. encode_path(collection) .. "/indexes", {
    name = name,
    field = field,
    unique = unique == true,
  })
end

function Client:put(collection, key, value, expected_version)
  if expected_version ~= nil then
    return self:put_checked(collection, key, value, expected_version)
  end
  local data, err = self:call("POST", "/v1/collections/" .. encode_path(collection) .. "/documents", {
    key = key,
    value = value,
  })
  if not data then return nil, err end
  return data.version
end

function Client:put_checked(collection, key, value, expected_version)
  local body = { value = value }
  if expected_version ~= nil then body.expected_version = expected_version end
  local data, err = self:call(
    "PUT",
    "/v1/collections/" .. encode_path(collection) .. "/documents/" .. encode_path(key),
    body
  )
  if not data then return nil, err end
  return data.version
end

function Client:get(collection, key)
  local data, err = self:call(
    "GET",
    "/v1/collections/" .. encode_path(collection) .. "/documents/" .. encode_path(key)
  )
  if not data then
    if err and err.code == "not_found" then
      return nil
    end
    return nil, err
  end
  return data.value
end

--- Fetch the full envelope (key, value, version, created_at, ...).
function Client:get_envelope(collection, key)
  return self:call(
    "GET",
    "/v1/collections/" .. encode_path(collection) .. "/documents/" .. encode_path(key)
  )
end

function Client:delete(collection, key, expected_version)
  local path = "/v1/collections/" .. encode_path(collection) .. "/documents/" .. encode_path(key)
  if expected_version then
    path = path .. "?expected_version=" .. tostring(expected_version)
  end
  local data, err = self:call("DELETE", path)
  if not data then return nil, err end
  return true
end

function Client:scan(collection, options)
  options = options or {}
  local query = { "limit=" .. tostring(options.limit or 100) }
  if options.prefix then query[#query + 1] = "prefix=" .. percent_encode(options.prefix) end
  if options.offset then query[#query + 1] = "offset=" .. tostring(options.offset) end
  if options.reverse then query[#query + 1] = "reverse=true" end
  local data, err = self:call(
    "GET",
    "/v1/collections/" .. encode_path(collection) .. "/documents?" .. table.concat(query, "&")
  )
  if not data then return nil, err end
  return data.documents or preppydb.array({})
end

function Client:count(collection)
  local info, err = self:collection(collection)
  if not info then return nil, err end
  return info.documents
end

function Client:batch(collection, operations)
  return self:call(
    "POST",
    "/v1/collections/" .. encode_path(collection) .. "/batch",
    { operations = preppydb.array(operations) }
  )
end

function Client:find(collection, index, value)
  local data, err = self:call(
    "GET",
    "/v1/collections/" .. encode_path(collection)
      .. "/indexes/" .. encode_path(index)
      .. "?value=" .. percent_encode(preppydb.encode(value))
  )
  if not data then return nil, err end
  return data.documents or preppydb.array({})
end

function Client:range(collection, index, from_value, to_value)
  local query = {}
  if from_value ~= nil then query[#query + 1] = "from=" .. percent_encode(preppydb.encode(from_value)) end
  if to_value ~= nil then query[#query + 1] = "to=" .. percent_encode(preppydb.encode(to_value)) end
  local data, err = self:call(
    "GET",
    "/v1/collections/" .. encode_path(collection) .. "/indexes/" .. encode_path(index)
      .. "?" .. table.concat(query, "&")
  )
  if not data then return nil, err end
  return data.rows or data.documents or preppydb.array({})
end

function Client:checkpoint()
  return self:call("POST", "/v1/admin/checkpoint")
end

function Client:compact()
  return self:call("POST", "/v1/admin/compact")
end

function Client:verify()
  local data, err = self:call("POST", "/v1/admin/verify")
  if not data and err and err.status == 409 then
    return { ok = false }, nil
  end
  return data, err
end

function Client:backup(path)
  return self:call("POST", "/v1/admin/backup", { path = path })
end

function Client:recovery()
  return self:call("GET", "/v1/admin/recovery")
end

function Client:bootstrap_personality()
  return self:call("POST", "/v1/personality/bootstrap")
end

function Client:personality_overview()
  return self:call("GET", "/v1/personality/overview")
end

function Client:personality_profiles()
  local data, err = self:call("GET", "/v1/personality/profiles")
  if not data then return nil, err end
  return data.profiles or preppydb.array({})
end

function Client:price_rules()
  local data, err = self:call("GET", "/v1/pricing/rules")
  if not data then return nil, err end
  return data.rules or preppydb.array({})
end

function Client:put_price_rule(rule)
  if type(rule) ~= "table" or not rule.item_id then
    return nil, { code = "invalid_argument", message = "rule needs an item_id", retryable = false }
  end
  return self:call("PUT", "/v1/pricing/rules/" .. encode_path(rule.item_id), rule)
end

function Client:tick(item, steps)
  return self:call("POST", "/v1/pricing/rules/" .. encode_path(item) .. "/tick", {
    steps = steps or 1,
  })
end

function Client:offer(item, ttl_ms)
  local body = { item_id = item }
  if ttl_ms then body.ttl_ms = ttl_ms end
  return self:call("POST", "/v1/pricing/offers", body)
end

function Client:purchase(offer_id, quantity)
  return self:call("POST", "/v1/pricing/purchases", {
    offer_id = offer_id,
    quantity = quantity or 1,
  })
end

function Client:price_history(item)
  local data, err = self:call("GET", "/v1/pricing/history/" .. encode_path(item))
  if not data then return nil, err end
  return data.history or preppydb.array({})
end

function Client:simulate(rule, steps, seed)
  return self:call("POST", "/v1/pricing/simulate", {
    rule = rule,
    steps = steps or 5,
    seed = seed,
  })
end

function Client:audit_verify()
  return self:call("GET", "/v1/pricing/audit/verify")
end

preppydb.Client = Client
preppydb.json = { encode = preppydb.encode, decode = preppydb.decode, null = preppydb.null }

return preppydb
