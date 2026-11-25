-- `/mcp auth <server>` completion: whether the token being typed is that
-- command's server argument, and the rows to offer for it. The list comes
-- from the host (`maki.mcp.servers`), the same snapshot the `/mcp` picker
-- shows, so a server that connected after startup is offered too.

local Mcp = {}

-- The command takes the whole line up to the token; extra spaces are fine,
-- the command parser splits on whitespace anyway.
local PREFIX = "^/mcp%s+auth%s+$"

-- True when {text} up to the token at {start} is a `/mcp auth` waiting for
-- its server name.
function Mcp.is_auth_arg(text, start)
  return text:sub(1, start):find(PREFIX) ~= nil
end

-- Rows for the server-name token: every configured server whose name contains
-- {query}, case-insensitive, name order.
function Mcp.rows(query, limit)
  local servers
  if maki.mcp and maki.mcp.servers then
    servers = maki.mcp.servers()
  end
  local needle = (query or ""):lower()
  local matches = {}
  for _, s in ipairs(servers or {}) do
    if needle == "" then
      matches[#matches + 1] = { name = s.name, highlights = {} }
    else
      local start, stop = s.name:lower():find(needle, 1, true)
      if start then
        matches[#matches + 1] = { name = s.name, highlights = { { start, stop } } }
      end
    end
  end
  table.sort(matches, function(a, b)
    return a.name < b.name
  end)
  local rows = {}
  for i = 1, math.min(limit, #matches) do
    local m = matches[i]
    rows[i] = {
      label = m.name,
      insert = m.name,
      highlights = m.highlights,
    }
  end
  return rows
end

return Mcp
