-- The path token under the cursor, the directory to list for it, and the rows
-- to offer. Pure: no host calls, so every rule is testable on a string.
--
-- Byte offsets throughout, the unit `maki.ui.input` and `maki.ui.input_edit`
-- speak. A token occupies `[start, cursor)`, which is the range an accept
-- writes over.

local Path = {}

-- A token ends at the last of these; `/` is not one, so `F:/SomeF`, `./../x`
-- and `~/foo` each stay a single token.
local DELIMITERS = "[ \t\r\n\"'=]"

-- The directory `maki.fs.dir` lists is absolute; the prefix filters it; the
-- display prefix is the directory as the user typed it, so a completion keeps
-- the `~/`, `./../` or `F:/` shape they wrote.
local GIT = ".git"

local function starts_with(text, prefix)
  return text:sub(1, #prefix) == prefix
end

-- Everything from the last delimiter to the cursor, and where it starts.
function Path.parse(text, cursor)
  local before = text:sub(1, cursor)
  local after = before:match("^.*" .. DELIMITERS .. "()")
  local start = after or 1
  return start - 1, before:sub(start)
end

-- Worth completing: it carries a separator, a home, a dot, a leading slash, or
-- a drive letter. A bare word does not, so Tab on one stays the mode toggle.
function Path.is_path(token)
  return token:find("[/\\]") ~= nil
    or token:sub(1, 1) == "~"
    or token:sub(1, 1) == "."
    or token:sub(1, 1) == "/"
    or token:match("^%a:") ~= nil
end

local function dirname(token)
  local _, last = token:find(".*/")
  if last then
    return token:sub(1, last)
  end
  return ""
end

local function basename(token)
  return token:match("[^/]*$")
end

local function expand_home(token, home)
  if starts_with(token, "~/") then
    return home .. "/" .. token:sub(3)
  end
  return token
end

local function is_absolute(path)
  return path:sub(1, 1) == "/" or path:match("^%a:/") ~= nil
end

-- {token} as typed → the directory to list, the prefix to filter by, and the
-- display prefix. A trailing slash lists that directory whole; a bare `~` lists
-- home; anything else splits into its directory and the name being typed.
function Path.resolve(token, cwd, home)
  if token == "~" then
    token = "~/"
  end
  local typed = token:gsub("\\", "/")

  local dir, prefix, disp
  if typed:sub(-1) == "/" then
    dir, prefix, disp = typed, "", typed
  else
    dir, prefix, disp = dirname(typed), basename(typed), dirname(typed)
  end

  local expanded = expand_home(dir, home)
  if not is_absolute(expanded) then
    expanded = (cwd:gsub("\\", "/"):sub(-1) == "/" and cwd .. expanded or cwd .. "/" .. expanded)
  end
  return { dir = expanded, prefix = prefix, disp = disp }
end

-- The listed entries, filtered by the prefix and ordered directories first.
-- Directories carry a trailing slash so the next Tab descends into them.
function Path.entries(raw, disp, prefix, limit)
  local needle = prefix:lower()
  local rows = {}
  for _, entry in ipairs(raw) do
    local name, kind = entry[1], entry[2]
    if name ~= GIT and (needle == "" or name:lower():sub(1, #needle) == needle) then
      local is_dir = kind == "directory"
      local label = is_dir and name .. "/" or name
      rows[#rows + 1] = {
        label = label,
        insert = disp .. label,
        is_dir = is_dir,
        highlights = prefix == "" and {} or { { 1, #prefix } },
      }
    end
  end
  table.sort(rows, function(a, b)
    if a.is_dir ~= b.is_dir then
      return a.is_dir
    end
    return a.label:lower() < b.label:lower()
  end)
  local out = {}
  for i = 1, math.min(limit, #rows) do
    out[i] = rows[i]
  end
  return out
end

return Path
