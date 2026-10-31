local M = {}
local guards = require("maki.guards")

local FILE_SYMBOL = "<file>"
local YIELD_INTERVAL = 256
local DECLARATIONS = {
  function_item = true,
  function_signature_item = true,
  struct_item = true,
  enum_item = true,
  union_item = true,
  trait_item = true,
  impl_item = true,
  mod_item = true,
  const_item = true,
  static_item = true,
  type_item = true,
  function_definition = true,
  class_definition = true,
  function_declaration = true,
  generator_function_declaration = true,
  class_declaration = true,
  method_definition = true,
  method_declaration = true,
  method_signature = true,
  interface_declaration = true,
  type_alias_declaration = true,
  enum_declaration = true,
  variable_declarator = true,
  type_spec = true,
  type_alias = true,
  method_elem = true,
}

local function field(node, name)
  return node:field(name)[1]
end

local function text(node, source)
  return maki.treesitter.get_node_text(node, source)
end

local function compact(value)
  return value:gsub("%s+", " "):match("^%s*(.-)%s*$")
end

local function declaration_name(node, source, lang, parent)
  local kind = node:type()
  if kind == "variable_declarator" and parent.name then
    return nil
  end
  if kind == "impl_item" then
    local target = field(node, "type")
    if not target then
      return nil
    end
    local trait = field(node, "trait")
    return "impl " .. (trait and (compact(text(trait, source)) .. " for ") or "") .. compact(text(target, source))
  end
  if not DECLARATIONS[kind]
    and not kind:match("_declaration$")
    and not kind:match("_definition$")
    and not kind:match("_item$")
  then
    return nil
  end
  local name = field(node, "name")
  if not name then
    return nil
  end
  local result = compact(text(name, source))
  if lang == "go" and kind == "method_declaration" then
    local receiver = field(node, "receiver")
    local parameter = receiver and receiver:named_child(0)
    local receiver_type = parameter and field(parameter, "type")
    if not receiver_type then
      return nil
    end
    result = compact(text(receiver_type, source)):gsub("^%*", "") .. "::" .. result
  end
  return result
end

local function masked(source, entry)
  local pieces = {}
  local cursor = entry.first
  for _, child in ipairs(entry.children) do
    pieces[#pieces + 1] = source:sub(cursor + 1, child.first)
    pieces[#pieces + 1] = "\0" .. child.name .. "\0"
    cursor = child.last
  end
  pieces[#pieces + 1] = source:sub(cursor + 1, entry.last)
  return table.concat(pieces)
end

local function declarations(source, lang)
  local parser = maki.treesitter.get_string_parser(source, lang)
  if not parser then
    return nil
  end
  local trees = parser:parse()
  local root = trees[1] and trees[1]:root()
  if not root or root:has_error() then
    return nil
  end
  local result = { by_name = {}, entries = {}, first = 0, last = #source, children = {} }
  local visited = 0
  local ambiguous = false
  local function walk(node, parent, wrapper)
    visited = visited + 1
    if visited % YIELD_INTERVAL == 0 then
      maki.async.sleep(0)
    end
    local name = declaration_name(node, source, lang, parent)
    local entry = parent
    if name then
      name = parent.name and (parent.name .. "::" .. name) or name
      if result.by_name[name] then
        ambiguous = true
        return
      end
      local range = maki.treesitter.get_range(wrapper or node)
      entry = { name = name, kind = node:type(), first = range[3], last = range[6], children = {}, parent = parent }
      result.by_name[name] = entry
      result.entries[#result.entries + 1] = entry
      parent.children[#parent.children + 1] = entry
    end
    local decorated = node:type() == "decorated_definition" and node or nil
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      walk(child, entry, decorated and (kind == "function_definition" or kind == "class_definition") and decorated or nil)
    end
    if name then
      entry.own = masked(source, entry)
    end
  end
  walk(root, result)
  if ambiguous then
    return nil
  end
  result.own = masked(source, result)
  return result
end

local function changes(path, before, after)
  if before == after then
    return {}, nil, nil
  end
  local lang = maki.treesitter.language_for_path(path)
  local old = lang and declarations(before, lang)
  local new = lang and declarations(after, lang)
  if not old or not new or old.own ~= new.own then
    return { FILE_SYMBOL }, old, new
  end
  local names = {}
  local changed = {}
  local function add(name)
    if not changed[name] then
      changed[name] = true
      names[#names + 1] = name
    end
  end
  local function add_tree(entry)
    add(entry.name)
    for _, child in ipairs(entry.children) do
      add_tree(child)
    end
  end
  for _, entry in ipairs(old.entries) do
    local next_entry = new.by_name[entry.name]
    if not next_entry or entry.kind ~= next_entry.kind or entry.own ~= next_entry.own then
      add_tree(entry)
      if next_entry then
        add_tree(next_entry)
      end
    end
  end
  for _, entry in ipairs(new.entries) do
    if not old.by_name[entry.name] then
      add_tree(entry)
    end
  end
  table.sort(names)
  return names, old, new
end

local function drift_error(path, symbol)
  return "stale edit: symbol " .. symbol .. " in " .. path .. " changed since it was read; read the file again"
end

function M.apply(ctx, path, before, after)
  local names, old, new = changes(path, before, after)
  local ok, lock_err = maki.agent.lock_symbols(ctx, path, names)
  if not ok then
    return nil, lock_err
  end
  local current, read_err = maki.fs.read(path)
  if read_err then
    return nil, "read error: " .. tostring(read_err)
  end
  if current == before then
    local accepted, err = guards.check(path, current, after)
    if not accepted then return nil, err end
    return after
  end
  if #names == 0 then
    local accepted, err = guards.check(path, current, current)
    if not accepted then return nil, err end
    return current
  end
  if names[1] == FILE_SYMBOL or not old or not new then
    return nil, drift_error(path, FILE_SYMBOL)
  end
  local now = declarations(current, maki.treesitter.language_for_path(path))
  if not now then
    return nil, drift_error(path, FILE_SYMBOL)
  end
  local touched = {}
  for _, name in ipairs(names) do
    touched[name] = true
    local baseline, candidate, live = old.by_name[name], new.by_name[name], now.by_name[name]
    if not baseline or not candidate or not live or baseline.kind ~= live.kind
      or before:sub(baseline.first + 1, baseline.last) ~= current:sub(live.first + 1, live.last)
    then
      return nil, drift_error(path, name)
    end
  end
  local splices = {}
  for _, name in ipairs(names) do
    local baseline = old.by_name[name]
    local parent = baseline.parent
    local covered = false
    while parent and parent.name do
      if touched[parent.name] then
        covered = true
        break
      end
      parent = parent.parent
    end
    if not covered then
      local live, candidate = now.by_name[name], new.by_name[name]
      splices[#splices + 1] = { first = live.first, last = live.last, text = after:sub(candidate.first + 1, candidate.last) }
    end
  end
  table.sort(splices, function(a, b)
    return a.first > b.first
  end)
  local live_before = current
  for _, splice in ipairs(splices) do
    current = current:sub(1, splice.first) .. splice.text .. current:sub(splice.last + 1)
  end
  local accepted, err = guards.check(path, live_before, current)
  if not accepted then return nil, err end
  return current
end

function M.check(ctx, path, before, after)
  local names = changes(path, before, after)
  local ok, err = maki.agent.lock_symbols(ctx, path, names)
  if not ok then return nil, err end
  return guards.check(path, before, after)
end

return M
