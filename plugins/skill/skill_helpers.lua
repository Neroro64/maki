local LINE_FMT = "%4d | %s"
local LINE_PAT = "^%s*%d+ | (.*)$"

local M = {}

function M.parse_frontmatter(content)
  local rest = content:match("^%s*%-%-%-\n(.*)")
  if not rest then
    return {}, content
  end
  local end_pos = rest:find("\n%-%-%-")
  if not end_pos then
    return {}, content
  end
  local yaml_str = rest:sub(1, end_pos)
  local body = rest:sub(end_pos + 4):match("^%s*(.-)%s*$")
  local fm, _ = maki.yaml.decode(yaml_str)
  if not fm then
    fm = {}
  end
  return fm, body
end

function M.build_skill_list(skills)
  local sorted = M.sorted(skills)
  local lines = {}
  for _, s in ipairs(sorted) do
    if s.enabled ~= false then
      lines[#lines + 1] = "- " .. s.name .. ": " .. s.description
    end
  end

  if #lines == 0 then
    return "\n\n<available_skills>\nNo skills available.\n</available_skills>"
  end

  return "\n\n<available_skills>\n" .. table.concat(lines, "\n") .. "\n</available_skills>"
end

-- `parse_output` must stay the exact inverse: the view is drawn from
-- this text, live and on restore.
function M.format_output(location, content)
  local lines = { location }
  for i, line in ipairs(maki.split(content, "\n")) do
    lines[i + 1] = string.format(LINE_FMT, i, line)
  end
  return table.concat(lines, "\n")
end

function M.parse_output(output)
  local location, rest = output:match("^([^\n]*)\n(.*)$")
  if not location then
    return nil
  end
  local lines = {}
  for i, line in ipairs(maki.split(rest, "\n")) do
    lines[i] = line:match(LINE_PAT)
    if not lines[i] then
      return nil
    end
  end
  return location, table.concat(lines, "\n")
end

-- The skills sorted by name, as a list. The registry and the picker both want
-- a stable order, and a pairs() walk gives none.
function M.sorted(skills)
  local out = {}
  for _, s in pairs(skills) do
    out[#out + 1] = s
  end
  table.sort(out, function(a, b)
    return a.name < b.name
  end)
  return out
end

-- Rows for `/skill:<prefix>` completion: every enabled skill whose name starts
-- with the prefix, case-insensitive. The insert is the whole token, so the
-- popup writes `/skill:name` over `/skill:pre`.
function M.skill_rows(skills, prefix, limit)
  local needle = prefix:lower()
  local rows = {}
  for _, s in ipairs(M.sorted(skills)) do
    if s.enabled ~= false and (needle == "" or s.name:lower():sub(1, #needle) == needle) then
      rows[#rows + 1] = {
        label = s.name,
        insert = "/skill:" .. s.name,
        highlights = prefix == "" and {} or { { 1, #prefix } },
      }
      if #rows >= limit then
        break
      end
    end
  end
  return rows
end

-- Flip {name} in the disabled set in place and return the set, so the picker's
-- live key handler can rebuild its rows from the same table it toggled.
function M.toggle_disabled(disabled, name)
  if disabled[name] then
    disabled[name] = nil
  else
    disabled[name] = true
  end
  return disabled
end

return M
