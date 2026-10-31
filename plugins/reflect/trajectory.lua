local M = {}

local MAX_TEXT = 1024
local MAX_INPUT = 2048
local MAX_TRAJECTORY = 24576
local MAX_REWIND = 12288
local MAX_SOURCES = 4
local MAX_SOURCE_BYTES = 65536
local MAX_SOURCE_LINES = 80
local MAX_SOURCE_LINE = 160

local function bounded(text, limit)
  if type(text) ~= "string" then
    return ""
  end
  if #text <= limit then
    return text
  end
  local finish = limit
  while finish > 0 and text:byte(finish + 1) >= 128 and text:byte(finish + 1) < 192 do
    finish = finish - 1
  end
  return text:sub(1, finish) .. " [truncated]"
end

local function correction(text)
  text = text:lower()
  for _, word in ipairs({ "wrong", "incorrect", "instead", "don't", "do not", "cannot", "bad import", "not valid" }) do
    if text:find(word, 1, true) then
      return true
    end
  end
  return false
end

local function result_text(content)
  if type(content) == "string" then
    return bounded(content, MAX_TEXT)
  end
  local parts = {}
  if type(content) == "table" then
    for _, block in ipairs(content) do
      if block.type == "text" then
        parts[#parts + 1] = bounded(block.text, MAX_TEXT)
      end
    end
  end
  return bounded(table.concat(parts, "\n"), MAX_TEXT)
end

function M.cache(text)
  return bounded(text, MAX_REWIND)
end

function M.collect(messages, rewind, args, root)
  local lines, calls, edits, paths = {}, {}, {}, {}
  local path_order = {}
  local failed, corrected, recovered, bytes = false, false, false, 0
  local function add(text)
    if text == "" then
      return
    end
    while #lines > 0 and bytes + #text > MAX_TRAJECTORY do
      bytes = bytes - #table.remove(lines, 1)
    end
    lines[#lines + 1] = text
    bytes = bytes + #text
  end
  local function remember(path, input)
    for index, previous in ipairs(path_order) do
      if previous == path then
        table.remove(path_order, index)
        break
      end
    end
    path_order[#path_order + 1] = path
    paths[path] = input
    if #path_order > MAX_SOURCES then
      paths[table.remove(path_order, 1)] = nil
    end
  end
  for _, message in ipairs(messages) do
    for _, block in ipairs(message.content or {}) do
      if block.type == "text" and (message.kind == nil or message.kind == "turn") and message.display_text ~= ""
        and (message.role == "user" or message.role == "assistant")
      then
        local text = bounded(block.text, MAX_TEXT)
        if message.role == "user" and correction(text) then
          corrected = true
        end
        add(message.role .. ": " .. text)
      elseif block.type == "tool_use" then
        local input = block.input
        calls[block.id] = block.name
        local compact = {}
        if type(input) == "table" then
          for _, key in ipairs({ "path", "old_string", "new_string", "content", "command", "cmd" }) do
            if type(input[key]) == "string" then
              compact[key] = bounded(input[key], MAX_INPUT)
            end
          end
          if (block.name == "edit" or block.name == "write") and type(input.path) == "string" then
            edits[block.id] = input
          end
        end
        add("tool " .. block.name .. ": " .. (maki.json.encode(compact) or "{}"))
        if type(input) == "table" and (block.name == "edit" or block.name == "write") then
          for _, key in ipairs({ "old_string", "new_string", "content" }) do
            if type(input[key]) == "string" then
              add(key .. " source snippet:\n" .. bounded(input[key], MAX_INPUT))
            end
          end
        end
      elseif block.type == "tool_result" then
        local is_error = block.is_error == true
        recovered = recovered or (failed and not is_error)
        failed = failed or is_error
        add("result " .. (calls[block.tool_use_id] or "tool") .. (is_error and " [error]: " or " [ok]: ") .. (is_error and result_text(block.content) or "completed"))
        local edit = edits[block.tool_use_id]
        if edit and not is_error then
          remember(edit.path, edit)
        end
      end
    end
  end
  local current_paths = {}
  for _, path in ipairs(path_order) do
    current_paths[path] = true
  end
  rewind = M.cache(rewind)
  if rewind ~= "" then
    add("Latest discarded rewind trajectory:\n" .. rewind)
    failed = failed or rewind:find("[error]", 1, true) ~= nil
    corrected = corrected or correction(rewind)
    for line in rewind:gmatch("[^\n]+") do
      local name, json = line:match("^tool %S+ (%S+) input: (.+)$")
      if name == "edit" or name == "write" then
        local input = maki.json.decode(json)
        if type(input) == "table" and type(input.path) == "string" then
          if not current_paths[input.path] and (paths[input.path] or #path_order < MAX_SOURCES) then
            remember(input.path, input)
          end
          for _, key in ipairs({ "old_string", "new_string", "content" }) do
            if type(input[key]) == "string" then
              add("Rewind " .. key .. " source snippet:\n" .. bounded(input[key], MAX_INPUT))
            end
          end
        end
      end
    end
  end
  args = bounded(args, MAX_TEXT)
  if args:match("%S") then
    add("Explicit user correction: " .. args)
    corrected = true
  end
  local grounded_fix = false
  local fixed_sources = {}
  for _, path in ipairs(path_order) do
    local edit = paths[path]
    local absolute = maki.fs.joinpath(root, path)
    local metadata = maki.fs.metadata(absolute)
    local source
    if metadata and metadata.is_file and metadata.size <= MAX_SOURCE_BYTES then
      source = maki.fs.read(absolute)
    else
      local page = maki.fs.read_lines(absolute, { limit = MAX_SOURCE_LINES, max_line_bytes = MAX_SOURCE_LINE, count = false })
      source = page and table.concat(page.lines, "\n")
    end
    if source then
      local fix = edit.new_string or edit.content
      local first = type(fix) == "string" and fix ~= "" and source:find(fix:gsub("\n$", ""), 1, true)
      if first then
        grounded_fix = true
      end
      add("Current final source " .. path .. ":\n" .. bounded(source:sub(first or 1), MAX_INPUT))
      fixed_sources[#fixed_sources + 1] = source
    end
  end
  local text = table.concat(lines, "\n")
  failed = failed and text:find("[error]", 1, true) ~= nil
  if not (failed or corrected) or not (grounded_fix or recovered) then
    return nil, "no meaningful error or correction evidence"
  end
  return text, nil, fixed_sources
end

function M.grounded(proposal, evidence, fixed_sources)
  if type(proposal) ~= "table" or proposal.kind ~= "guard" then
    return true
  end
  local guard = proposal.guard
  if type(guard) ~= "table" or type(guard.failed_example) ~= "string" or type(guard.fixed_example) ~= "string" then
    return false
  end
  if guard.failed_example == "" or guard.fixed_example == "" or guard.failed_example == guard.fixed_example
    or not evidence:find(guard.failed_example, 1, true) or not evidence:find(guard.fixed_example, 1, true)
  then
    return false
  end
  for _, source in ipairs(fixed_sources) do
    if source:find(guard.fixed_example, 1, true) then
      return true
    end
  end
  return false
end

return M
