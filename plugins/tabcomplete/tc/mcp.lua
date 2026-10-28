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

-- Rows for the server-name token: every configured server whose name starts
-- with {prefix}, case-insensitive, name order.
function Mcp.rows(prefix, limit)
  local servers
  if maki.mcp and maki.mcp.servers then
    servers = maki.mcp.servers()
  end
  local needle = (prefix or ""):lower()
  local names = {}
  for _, s in ipairs(servers or {}) do
    if needle == "" or s.name:lower():sub(1, #needle) == needle then
      names[#names + 1] = s.name
    end
  end
  table.sort(names)
  local rows = {}
  for i = 1, math.min(limit, #names) do
    local name = names[i]
    rows[i] = {
      label = name,
      insert = name,
      highlights = prefix == "" and {} or { { 1, #prefix } },
    }
  end
  return rows
end

return Mcp
