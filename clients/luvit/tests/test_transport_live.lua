-- Real Luvit HTTP transport against a local ephemeral server; no external service.
package.path = "./clients/luvit/?.lua;" .. package.path
local http = require("http")
local preppydb = require("../preppydb")
local server
server = http.createServer(function(_, response)
  local body = '{"ok":true}'
  response:writeHead(200, { ["Content-Length"] = #body, ["Content-Type"] = "application/json" })
  response:finish(body)
end)
server:listen(0, "127.0.0.1", function()
  local address = server:address()
  local url = "http://127.0.0.1:" .. address.port
  local client = assert(preppydb.new({ url = url, max_response_bytes = 64 }))
  client:call_async("GET", "/", nil, function(value, err)
    assert(not err and value.ok == true, "normal Luvit request failed")
    local bounded = assert(preppydb.new({ url = url, max_response_bytes = 4 }))
    bounded:call_async("GET", "/", nil, function(_, oversized)
      assert(oversized and oversized.code == "transport_error", "oversized response accepted")
      print("real Luvit transport: normal and oversized responses passed")
      server:close()
    end)
  end)
end)
