-- The disabled-skills store: a JSON list in the state dir, the source of truth
-- for `/skills` toggles. The skill tool, the completion popup and the picker
-- all read it, so a skill you turn off stays off across restarts.

local M = {}

local FILE = "skills.json"

local function path()
  local state = maki.env.state_dir()
  if not state then
    return nil
  end
  return maki.fs.joinpath(state, FILE)
end

-- The disabled set as { [name] = true }. A missing or unreadable file is an
-- empty set: nothing is disabled, which is the safe default.
function M.load()
  local p = path()
  if not p then
    return {}
  end
  local text = maki.fs.read(p)
  if not text then
    return {}
  end
  local list = maki.json.decode(text)
  local disabled = {}
  if type(list) == "table" then
    for _, name in ipairs(list) do
      if type(name) == "string" then
        disabled[name] = true
      end
    end
  end
  return disabled
end

function M.save(disabled)
  local p = path()
  if not p then
    return "no state dir"
  end
  local names = {}
  for name in pairs(disabled) do
    names[#names + 1] = name
  end
  table.sort(names)
  local _, err = maki.fs.write(p, maki.json.encode(names))
  return err
end

return M
