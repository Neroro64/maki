local M = {}

local MAX_GUARDS = 32
local MAX_POLICY_BYTES = 65536
local MAX_PATTERN_BYTES = 4096
local MAX_TOTAL_PATTERN_BYTES = 16384
local MAX_EXAMPLE_BYTES = 16384
local MAX_SOURCE_BYTES = 2097152
local MAX_MATCHES = 8192
local MAX_RULE_BYTES = 240
local MAX_RULE_WORDS = 40
local MAX_RULE_TOKENS = 512
local IMPERATIVES = {
  avoid = true, never = true, use = true, keep = true, reject = true,
  require = true, read = true, write = true, check = true, validate = true,
  import = true, call = true, preserve = true, run = true, replace = true,
  limit = true, prefer = true, remove = true, only = true,
}
local cached_policy

local function object(value, allowed)
  if type(value) ~= "table" or getmetatable(value) then
    return nil, "expected a plain policy object"
  end
  for key in pairs(value) do
    if not allowed[key] then
      return nil, "unknown policy field: " .. tostring(key)
    end
  end
  return true
end

local function rule_score(rule)
  if type(rule) ~= "string" or #rule == 0 or #rule > MAX_RULE_BYTES or rule:find("[%c]")
    or rule:match("^%s") or rule:match("%s$")
  then
    return nil, "rule must be one operational line of at most 240 bytes"
  end
  local words = 0
  for _ in rule:gmatch("%S+") do
    words = words + 1
  end
  local first = (rule:match("^(%a+)") or ""):lower()
  local imperative = IMPERATIVES[first] or rule:lower():match("^do not ")
  local concrete = rule:match("[%w_]+[./:][%w_./:]+")
    or rule:match("`[%w_]+%([^`]*%)`")
  if words < 4 or words > MAX_RULE_WORDS or not imperative or not concrete then
    return nil, "rule must be a specific imperative naming a concrete API, path or operation, not a platitude"
  end
  return math.max(words, math.ceil(#rule / 4))
end

local function file_regex(pattern)
  if type(pattern) ~= "string" or #pattern == 0 or #pattern > 240
    or pattern:find("[^%w_./*?%-]") or pattern:sub(1, 1) == "/"
    or pattern:find("..", 1, true)
  then
    return nil, "file_pattern must be a bounded relative glob using *, ** or ?"
  end
  local parts = { pattern:find("/", 1, true) and "^" or "(?:^|/)" }
  local i = 1
  while i <= #pattern do
    local ch = pattern:sub(i, i)
    if ch == "*" and pattern:sub(i + 1, i + 1) == "*" then
      if pattern:sub(i + 2, i + 2) == "/" then
        parts[#parts + 1] = "(?:.*/)?"
        i = i + 3
      else
        parts[#parts + 1] = ".*"
        i = i + 2
      end
    elseif ch == "*" then
      parts[#parts + 1] = "[^/]*"
      i = i + 1
    elseif ch == "?" then
      parts[#parts + 1] = "[^/]"
      i = i + 1
    else
      parts[#parts + 1] = ch == "." and "\\." or ch
      i = i + 1
    end
  end
  parts[#parts + 1] = "$"
  return maki.regex(table.concat(parts))
end

local function findings(compiled, source)
  if #source > MAX_SOURCE_BYTES then
    return nil, "guard source exceeds 2-MiB budget"
  end
  if compiled.data.kind == "regex" then
    local ranges, err = compiled.matcher:ranges(source)
    if not ranges then return nil, err end
    local result = {}
    for _, range in ipairs(ranges) do
      if range[1] == range[2] then
        return nil, "guard cannot use zero-width matches"
      end
      result[#result + 1] = { pattern = 1, parts = { { range[1], range[2], 1 } } }
    end
    return result
  end
  local parser, err = maki.treesitter.get_string_parser(source, compiled.data.language)
  if not parser then return nil, err or "cannot parse guard source" end
  local root = parser:parse()[1]:root()
  local result = {}
  for pattern, captures in compiled.matcher:iter_matches(root, source) do
    local parts = {}
    for capture, nodes in pairs(captures) do
      for _, node in ipairs(nodes) do
        local range = maki.treesitter.get_range(node)
        if range[3] == range[6] then return nil, "AST guard cannot capture zero-width nodes" end
        parts[#parts + 1] = { range[3], range[6], capture }
      end
    end
    if #parts == 0 then return nil, "AST guard must capture a nonempty source finding" end
    table.sort(parts, function(a, b)
      if a[3] ~= b[3] then return a[3] < b[3] end
      if a[1] ~= b[1] then return a[1] < b[1] end
      return a[2] < b[2]
    end)
    if #result == MAX_MATCHES then return nil, "guard match budget exceeded" end
    result[#result + 1] = { pattern = pattern, parts = parts }
    if #result % 256 == 0 then maki.async.sleep(0) end
  end
  return result
end

local function compile_guard(data)
  local ok, err = object(data, {
    kind = true, pattern = true, language = true, file_pattern = true,
    failed_example = true, fixed_example = true,
  })
  if not ok then return nil, err end
  if data.kind ~= "regex" and data.kind ~= "ast" then return nil, "unknown guard kind" end
  if type(data.pattern) ~= "string" or #data.pattern == 0 or #data.pattern > MAX_PATTERN_BYTES then
    return nil, "guard pattern must contain 1-4096 bytes"
  end
  if data.kind == "ast" and (type(data.language) ~= "string" or #data.language > 32) then
    return nil, "AST guard requires a language"
  end
  if data.language ~= nil and (type(data.language) ~= "string" or #data.language == 0 or #data.language > 32) then
    return nil, "guard language must be a bounded language name"
  end
  for _, key in ipairs({ "failed_example", "fixed_example" }) do
    if type(data[key]) ~= "string" or #data[key] == 0 or #data[key] > MAX_EXAMPLE_BYTES
      or not data[key]:find("%S")
    then
      return nil, "guard requires concrete failed/fixed examples of at most 16384 bytes"
    end
  end
  if data.failed_example == data.fixed_example then return nil, "guard examples must demonstrate a change" end
  local files
  files, err = file_regex(data.file_pattern)
  if not files then return nil, err end
  local matcher
  if data.kind == "regex" then
    matcher, err = maki.regex(data.pattern)
  else
    matcher, err = maki.treesitter.query.parse_strict(data.language, data.pattern)
  end
  if not matcher then return nil, err end
  local compiled = { data = data, files = files, matcher = matcher }
  local failed, fixed
  failed, err = findings(compiled, data.failed_example)
  if not failed then return nil, err end
  fixed, err = findings(compiled, data.fixed_example)
  if not fixed then return nil, err end
  if #failed == 0 then return nil, "guard misses the failed example" end
  if #fixed ~= 0 then return nil, "guard rejects the fixed example" end
  return compiled
end

local function compile_proposal(proposal)
  local ok, err = object(proposal, { kind = true, rule = true, guard = true })
  if not ok then return nil, err end
  if proposal.kind ~= "guard" and proposal.kind ~= "behavioral" then return nil, "unknown proposal kind" end
  local score
  score, err = rule_score(proposal.rule)
  if not score then return nil, err end
  if proposal.kind == "behavioral" then
    if proposal.guard ~= nil then return nil, "behavioral proposal cannot contain a guard" end
    return { rule = proposal.rule, score = score }
  end
  local compiled
  compiled, err = compile_guard(proposal.guard)
  if not compiled then return nil, err end
  compiled.rule = proposal.rule
  return compiled
end

local function read_optional(path, limit)
  local metadata, err = maki.fs.metadata(path)
  if err then return nil, err end
  if not metadata then return "" end
  local content
  content, err = maki.fs.read_bytes(path, { offset = 0, len = limit + 1 })
  if not content then return nil, err end
  return buffer.tostring(content)
end

local function load_policy(root)
  local source, err = read_optional(maki.fs.joinpath(root, ".maki", "guards.lua"), MAX_POLICY_BYTES)
  if not source then return nil, err end
  if source == "" then
    local metadata
    metadata, err = maki.fs.metadata(maki.fs.joinpath(root, ".maki", "guards.lua"))
    if err then return nil, err end
    if metadata then return nil, "empty stored guard policy" end
    return { version = 1, guards = {} }, {}
  end
  if #source > MAX_POLICY_BYTES then return nil, "stored guard policy exceeds source budget" end
  if cached_policy and cached_policy.root == root and cached_policy.source == source then
    return cached_policy.data, cached_policy.guards
  end
  local delimiter, start = source:match("^%s*return%s+%[(=*)%[()")
  if not delimiter then return nil, "stored guard policy must return a data-only long-bracket JSON literal" end
  local close = "]" .. delimiter .. "]"
  local finish = source:find(close, start, true)
  if not finish or not source:sub(finish + #close):match("^%s*;?%s*$") then
    return nil, "stored guard policy contains executable or malformed content"
  end
  local data
  data, err = maki.json.decode(source:sub(start, finish - 1))
  if not data then return nil, err end
  local ok
  ok, err = object(data, { version = true, guards = true })
  if not ok then return nil, err end
  local sequence = type(data.guards) == "table" and maki.json.encode(data.guards)
  if data.version ~= 1 or not sequence or sequence:sub(1, 1) ~= "[" then
    return nil, "unsupported stored guard policy version or non-array guards"
  end
  local count = 0
  for key in pairs(data.guards) do
    if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then return nil, "stored guards must be an array" end
    count = count + 1
  end
  if count ~= #data.guards or count > MAX_GUARDS then return nil, "stored guard count budget or array shape invalid" end
  local compiled, patterns, seen = {}, 0, {}
  for _, proposal in ipairs(data.guards) do
    if proposal.kind ~= "guard" then return nil, "stored policy contains a non-guard" end
    local guard
    guard, err = compile_proposal(proposal)
    if not guard then return nil, err end
    patterns = patterns + #guard.data.pattern
    if patterns > MAX_TOTAL_PATTERN_BYTES then return nil, "stored pattern budget exceeded" end
    local identity = table.concat({ guard.data.kind, guard.data.kind == "ast" and guard.data.language or "", guard.data.file_pattern, guard.data.pattern }, "\0")
    if seen[identity] then return nil, "stored policy contains duplicate guards" end
    seen[identity] = true
    compiled[#compiled + 1] = guard
  end
  cached_policy = { root = root, source = source, data = data, guards = compiled }
  return data, compiled
end

local function locked(root, fn)
  local permit, err = maki.fs.write_lock(root)
  if not permit then return nil, err end
  maki.async.on_cancel(function() permit:release() end)
  local ok, value, detail = pcall(fn)
  permit:release()
  if not ok then return nil, tostring(value) end
  return value, detail
end

function M.save(root, proposal)
  if type(root) ~= "string" or root == "" then return nil, "guard save requires a project root" end
  root = maki.fs.normalize(root)
  local encoded, snapshot_err = maki.json.encode(proposal)
  if not encoded then return nil, snapshot_err end
  if #encoded > MAX_POLICY_BYTES then return nil, "proposal exceeds source budget" end
  proposal, snapshot_err = maki.json.decode(encoded)
  if not proposal then return nil, snapshot_err end
  local ok, compiled, err = pcall(compile_proposal, proposal)
  if not ok then return nil, tostring(compiled) end
  if not compiled then return nil, err end
  return locked(root, function()
    local data, guards = load_policy(root)
    if not data then return nil, "invalid stored guard policy: " .. tostring(guards) end
    local path = maki.fs.joinpath(root, ".maki", proposal.kind == "guard" and "guards.lua" or "rules.md")
    local output, append_text
    if proposal.kind == "behavioral" then
      local previous, read_err = read_optional(path, MAX_RULE_TOKENS * 4)
      if not previous then return nil, read_err end
      for line in previous:gmatch("[^\r\n]+") do
        if line:gsub("^%s*%-%s*", ""):lower() == proposal.rule:lower() then return nil, "duplicate behavioral rule" end
      end
      append_text = (previous ~= "" and previous:sub(-1) ~= "\n" and "\n" or "") .. "- " .. proposal.rule .. "\n"
      output = previous .. append_text
      local words = 0
      for _ in output:gmatch("%S+") do words = words + 1 end
      if math.max(words, math.ceil(#output / 4)) > MAX_RULE_TOKENS then
        return nil, "behavioral rules exceed the 512 estimated-token budget"
      end
    else
      if #guards == MAX_GUARDS then return nil, "guard count budget exceeded" end
      local patterns = #compiled.data.pattern
      for _, guard in ipairs(guards) do
        patterns = patterns + #guard.data.pattern
        if guard.rule:lower() == compiled.rule:lower() then return nil, "duplicate guard rule" end
        if guard.data.kind == compiled.data.kind and guard.data.pattern == compiled.data.pattern
          and (guard.data.kind == "regex" or guard.data.language == compiled.data.language)
          and guard.data.file_pattern == compiled.data.file_pattern
        then return nil, "duplicate guard" end
        if guard.data.file_pattern == compiled.data.file_pattern
          and (guard.data.kind == "regex" or guard.data.language == compiled.data.language)
        then
          local covered, coverage_err = findings(guard, compiled.data.failed_example)
          if not covered then return nil, coverage_err end
          if #covered ~= 0 then return nil, "no-op proposal: failed example is already guarded" end
        end
      end
      if patterns > MAX_TOTAL_PATTERN_BYTES then return nil, "total guard pattern budget exceeded" end
      local persisted = { version = 1, guards = {} }
      for i, previous in ipairs(data.guards) do persisted.guards[i] = previous end
      persisted.guards[#persisted.guards + 1] = proposal
      local encoded, encode_err = maki.json.encode(persisted)
      if not encoded then return nil, encode_err end
      local delimiter = ""
      while encoded:find("]" .. delimiter .. "]", 1, true) do delimiter = delimiter .. "=" end
      output = "return [" .. delimiter .. "[" .. encoded .. "]" .. delimiter .. "]\n"
      if #output > MAX_POLICY_BYTES then return nil, "guard source budget exceeded" end
    end
    local made, mkdir_err = maki.fs.mkdir(maki.fs.joinpath(root, ".maki"), { parents = true })
    if not made then return nil, mkdir_err end
    local written, write_err
    if append_text then
      written, write_err = maki.fs.append(path, append_text)
    else
      written, write_err = maki.fs.atomic_write(path, output)
    end
    if not written then return nil, write_err end
    return path
  end)
end

local function match_key(match, equal)
  local key = { tostring(match.pattern) }
  for _, part in ipairs(match.parts) do
    local first, last = part[1], part[2]
    if equal then
      local low, high, found = 1, #equal, false
      while low <= high do
        local middle = math.floor((low + high) / 2)
        local span = equal[middle]
        if first < span[2] then
          high = middle - 1
        elseif first >= span[2] + span[3] then
          low = middle + 1
        else
          if last <= span[2] + span[3] then
            first = span[1] + first - span[2]
            last = first + part[2] - part[1]
            found = true
          end
          break
        end
      end
      if not found then return nil end
    end
    key[#key + 1] = part[3] .. ":" .. first .. ":" .. last
  end
  return table.concat(key, ";")
end

local function check_policy(root, path, before, after)
  local data, guards = load_policy(root)
  if not data then return nil, "invalid stored guard policy: " .. tostring(guards) end
  if #guards == 0 or before == after then return true end
  local relative = maki.fs.relpath(root, path):gsub("\\", "/")
  local equal
  for _, guard in ipairs(guards) do
    if guard.files:match_str(relative) then
      local previous, err = findings(guard, before)
      if not previous then return nil, err end
      local candidate
      candidate, err = findings(guard, after)
      if not candidate then return nil, err end
      if #candidate ~= 0 then
        if not equal then
          equal, err = maki.text.unchanged_ranges(before, after)
          if not equal then return nil, err end
        end
        local existing = {}
        for _, match in ipairs(previous) do existing[match_key(match)] = true end
        for _, match in ipairs(candidate) do
          local mapped = match_key(match, equal)
          if not mapped or not existing[mapped] then return nil, "guard rejected " .. path .. ": " .. guard.rule end
        end
      end
    end
  end
  return true
end

function M.check(path, before, after)
  local ok, value, err = pcall(function()
    path = maki.fs.normalize(path)
    for _, root in ipairs(maki.fs.parents(path)) do
      local metadata, read_err = maki.fs.metadata(maki.fs.joinpath(root, ".maki", "guards.lua"))
      if read_err then return nil, "cannot read guard policy: " .. tostring(read_err) end
      if metadata then
        local accepted, policy_err = locked(root, function() return check_policy(root, path, before, after) end)
        if not accepted then return nil, policy_err end
      end
    end
    return true
  end)
  if not ok then return nil, "guard check failed: " .. tostring(value) end
  return value, err
end

return M
