-- The skill registry: the discovered skills, published by the skill plugin so
-- other plugins (the Tab completion popup) can list them without re-scanning
-- the disk. Each plugin runs in its own Lua environment with its own `require`
-- cache, so sharing crosses plugin boundaries through a zero-capability slot
-- (`tabcomplete.skills`) rather than a module-local variable.

local Skills = {}

Skills.SLOT = "tabcomplete.skills"

local collect = nil
local fallback = {}

-- Declared by the consumer (`tabcomplete`) so the slot exists for `rows` to
-- call. Layers registered before `declare` attach automatically once it runs.
function Skills.declare()
  collect = maki.api.declare_slot(Skills.SLOT, function()
    return fallback
  end, { capability = {} })
end

-- Called by the skill plugin to supply the latest discovered skills list.
function Skills.publish(get_fn)
  maki.api.set_slot(Skills.SLOT, function(_prev)
    return get_fn()
  end)
end

-- Sets the local fallback list (used in unit specs that do not wire the slot).
function Skills.set(skills)
  fallback = skills or {}
end

function Skills.get()
  if not collect then
    return fallback
  end
  local list = collect()
  return type(list) == "table" and list or fallback
end

-- Rows for `/skill:<query>` completion: every enabled skill whose name
-- contains {query}, case-insensitive, name order. The insert is the whole
-- token, so the popup writes `/skill:name` over `/skill:query`.
function Skills.rows(query, limit)
  local needle = (query or ""):lower()
  local matches = {}
  for _, s in ipairs(Skills.get()) do
    if s.enabled ~= false then
      if needle == "" then
        matches[#matches + 1] = { name = s.name, highlights = {} }
      else
        local start, stop = s.name:lower():find(needle, 1, true)
        if start then
          matches[#matches + 1] = { name = s.name, highlights = { { start, stop } } }
        end
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
      insert = "/skill:" .. m.name,
      highlights = m.highlights,
    }
  end
  return rows
end

return Skills
