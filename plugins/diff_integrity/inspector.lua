local M = {}

local MAX_FILES = 256
local MAX_SOURCE = 524288
local MAX_TOTAL = 4194304
local MAX_NODES = 100000
local MAX_FINDINGS = 256
local MAX_GIT = 4194304
local GIT_TIMEOUT = 5000
local WARNING = "[DIFF INTEGRITY WARNING]"
local MARKER = "[DIFF INTEGRITY JUSTIFICATION]"
local SUPPORTED = { rust = true, javascript = true, typescript = true, tsx = true, python = true }
local DECL = {
  function_item = true, function_definition = true, function_declaration = true,
  method_definition = true, class_definition = true, class_declaration = true,
  struct_item = true, enum_item = true, trait_item = true, mod_item = true,
  impl_item = true, variable_declarator = true, let_declaration = true,
  field_declaration = true, property_signature = true, public_field_definition = true,
  parameter = true, required_parameter = true, optional_parameter = true,
  typed_parameter = true, const_item = true, static_item = true, type_item = true,
  type_alias_declaration = true, interface_declaration = true,
  arrow_function = true, function_expression = true, closure_expression = true,
}
local ANONYMOUS_FUNCTION = { arrow_function = true, function_expression = true, closure_expression = true }

-- Two independent integer rolling hashes bind identifiers to the entire current
-- normalized diff, not a previous warning, reply, or filename alone.
local function digest(parts)
  local a, b, n = 0, 0, 0
  for _, part in ipairs(parts) do
    for i = 1, #part do
      local byte = part:byte(i)
      a = (a * 31 + byte) % 4294967291
      b = (b * 131 + byte) % 4294967279
    end
    a = (a * 31 + 256) % 4294967291
    b = (b * 131 + 256) % 4294967279
    n = n + #part
  end
  return string.format("%08x%08x-%x", a, b, n)
end

local function normalize(source)
  return (source:gsub("\r\n", "\n"):gsub("\n+$", ""))
end

local function path_key(path)
  local key = maki.fs.normalize(path):gsub("\\", "/"):gsub("/+$", "")
  if key:match("^%a:/") then key = key:lower() end
  return key
end

local function git(snapshot, args, limit)
  local argv = { "git", "-C", snapshot.root, "-c", "core.quotepath=false", "-c",
    "core.autocrlf=false", "-c", "core.attributesFile=", "--no-pager" }
  for _, arg in ipairs(args) do argv[#argv + 1] = arg end
  local count, overflow, id = 0, false, nil
  local function received(_, line)
    count = count + #line + 1
    if count > limit then
      overflow = true
      if id then maki.fn.jobstop(id) end
    end
  end
  local err
  id, err = maki.fn.jobstart(argv, {
    cwd = snapshot.root, tail = 1024,
    env = { GIT_ATTR_SOURCE = snapshot.attributes, GIT_ATTR_NOSYSTEM = "1",
      GIT_TERMINAL_PROMPT = "0", GIT_CONFIG_COUNT = "0" },
    on_stdout = received, on_stderr = received,
  })
  if not id then return nil, "Git could not start: " .. tostring(err) end
  maki.async.on_cancel(function() maki.fn.jobstop(id) end)
  local result
  result, err = maki.fn.jobwait(id, GIT_TIMEOUT)
  if not result then
    maki.fn.jobstop(id)
    return nil, "Git inspection failed: " .. tostring(err)
  end
  maki.fn.jobforget(id)
  if overflow or result.truncated or type(result.stdout) ~= "string" or #result.stdout > limit then
    return nil, "Git output exceeded its capture/analysis budget"
  end
  if result.exit_code ~= 0 then
    return nil, "Git inspection exited " .. tostring(result.exit_code) .. ": " .. tostring(result.stderr):sub(1, 400)
  end
  return result.stdout
end

local function paths(output)
  if output ~= "" and output:sub(-1) ~= "\0" then return nil, "malformed Git NUL path list" end
  local result = {}
  for path in output:gmatch("([^%z]+)%z") do
    if path:sub(1, 1) == "/" or path:match("^%a:") or path:find("\0", 1, true)
      or path:find("[\r\n]") or path:match("^%.%./") or path:find("/../", 1, true) then
      return nil, "Git reported an unsafe relative path"
    end
    result[#result + 1] = path
  end
  return result
end

local function text(node, source)
  return maki.treesitter.get_node_text(node, source)
end

local function field(node, name, source)
  local nodes = node:field(name)
  return nodes[1] and text(nodes[1], source) or nil
end
local function syntax_text(node, source, depth)
  depth = depth or 0
  if depth > 128 then error("type/attribute syntax depth budget exceeded") end
  local kind = node:type()
  if kind:find("string", 1, true) or kind:find("comment", 1, true) then return "" end
  if node:child_count() == 0 then return text(node, source) end
  local parts = {}
  for index = 0, node:child_count() - 1 do
    parts[#parts + 1] = syntax_text(node:child(index), source, depth + 1)
  end
  return table.concat(parts)
end


local function line(node)
  local row = node:start()
  return row + 1
end

local function compact(value)
  return value and value:gsub("%s+", "") or ""
end

local function test_call(callee)
  return callee == "it" or callee == "test" or callee == "specify" or callee == "xit"
    or callee == "xtest" or callee:match("^it%.") or callee:match("^test%.")
end

local function assertion_call(callee)
  return callee == "assert" or callee:match("^assert%.") or callee == "expect"
    or callee:match("%.assert[%w_]*$")
end

local function declaration(node, source)
  if not DECL[node:type()] then return nil end
  local name = field(node, "name", source) or field(node, "pattern", source)
  if node:type() == "impl_item" then
    name = (field(node, "trait", source) or "") .. " for " .. (field(node, "type", source) or "")
  end
  -- Anonymous function-likes have no stable name, so they count under their
  -- enclosing declaration instead of colliding as duplicate callables.
  return name and node:type() .. ":" .. compact(name) or nil
end

local function is_wide(value, imported, language)
  local value = compact(value)
  for name in pairs(imported) do
    local position = 1
    while position <= #value do
      local first, last = value:find("%f[%w_]" .. name .. "%f[^%w_]", position)
      if not first then break end
      if value:sub(math.max(1, first - 2), first - 1) ~= "::" then return true end
      position = last + 1
    end
  end
  return ((language == "typescript" or language == "tsx") and value:match("%f[%w_]any%f[^%w_]") ~= nil)
    or value:find("serde_json::Value", 1, true) ~= nil
    or value:match("Box<dyn[^>]*%f[%w_]Any%f[^%w_]") ~= nil
end

local function analyze(path, source)
  local language = maki.treesitter.language_for_path(path)
  -- A language without an integrity grammar is out of scope, not a warning.
  if not SUPPORTED[language] then return { assertions = {}, tests = {}, risks = {}, types = {} } end
  if source:find("\0", 1, true) then return nil, "changed code is binary: " .. path end
  local parser, err = maki.treesitter.get_string_parser(source, language)
  if not parser then return nil, "cannot parse " .. path .. ": " .. tostring(err) end
  local trees = parser:parse()
  if not trees[1] then return nil, "parser produced no syntax tree: " .. path end
  local root = trees[1]:root()
  if root:has_error() then return nil, "invalid syntax prevents integrity analysis: " .. path end
  local result = { assertions = {}, tests = {}, risks = {}, types = {} }
  local imported = {}
  if language == "rust" then
    local query
    query, err = maki.treesitter.query.parse_strict("rust", "(use_declaration) @import")
    if not query then return nil, "cannot compile integrity import query: " .. tostring(err) end
    local function imports(node, prefix, depth)
      if depth > 128 then error("import syntax depth budget exceeded") end
      local kind = node:type()
      if kind == "use_as_clause" then
        local path = compact(field(node, "path", source))
        if prefix ~= "" and not path:find("::", 1, true) then path = prefix .. "::" .. path end
        local alias = field(node, "alias", source)
        if path == "serde_json::Value" and alias then imported[alias] = true end
        return
      elseif kind == "scoped_use_list" then
        local path = compact(field(node, "path", source))
        prefix = prefix == "" and path or prefix .. "::" .. path
      elseif kind == "scoped_identifier" then
        if compact(text(node, source)) == "serde_json::Value" then imported.Value = true end
        return
      elseif kind == "identifier" then
        if prefix == "serde_json" and text(node, source) == "Value" then imported.Value = true end
        return
      elseif kind == "use_wildcard" and compact(syntax_text(node, source)) == "serde_json::*" then
        imported.Value = true
        return
      end
      for _, child in ipairs(node:named_children()) do imports(child, prefix, depth + 1) end
    end
    for _, node in query:iter_captures(root, source) do
      imports(node, "", 0)
    end
  end
  local function counted(collection, key, node)
    local entry = collection[key]
    if not entry then entry = { count = 0, line = line(node) }; collection[key] = entry end
    entry.count = entry.count + 1
  end
  local function risk(kind, node, scope, identity)
    local range = maki.treesitter.get_range(node)
    result.risks[#result.risks + 1] = { kind = kind, symbol = scope, line = line(node),
      start = range[3], finish = range[6], identity = identity or compact(syntax_text(node, source)),
      evidence = text(node, source):sub(1, 240) }
  end
  local nodes, declarations = 0, {}
  local function walk(node, scope, depth)
    nodes = nodes + 1
    if nodes > MAX_NODES or depth > 128 then error("AST work budget exceeded: " .. path) end
    local kind = node:type()
    local identity = declaration(node, source)
    if identity then scope = scope .. "/" .. identity end
    if identity and (kind:find("function", 1, true) or kind == "method_definition" or kind == "closure_expression") then
      if declarations[scope] then error("ambiguous callable declaration prevents net integrity analysis: " .. scope) end
      declarations[scope] = true
    end
    local callee
    if kind == "call_expression" or kind == "call" then
      callee = compact(field(node, "function", source))
      if test_call(callee) then
        local arguments = node:field("arguments")
        local first = arguments[1] and arguments[1]:named_child(0)
        local name = first and text(first, source) or "<unnamed>"
        scope = scope .. "/test:" .. name
        counted(result.tests, scope, node)
      end
      if assertion_call(callee) then counted(result.assertions, scope, node) end
      if callee == "xit" or callee == "xtest" or callee:match("^it%.skip") or callee:match("^test%.skip")
        or callee:match("^pytest%.skip$") or callee:match("^pytest%.mark%.skip") then
        risk("introduced test skip", node, scope, callee)
      end
      if callee == "test.todo" or callee == "it.todo" then risk("introduced TODO stub", node, scope, callee) end
    elseif kind == "assert_statement" then
      counted(result.assertions, scope, node)
    elseif kind == "macro_invocation" then
      local name = compact(field(node, "macro", source))
      local stub = name:match("([%w_]+)$")
      if name:match("%f[%w_]assert[%w_]*$") or name:match("%f[%w_]debug_assert[%w_]*$") then
        counted(result.assertions, scope, node)
      elseif stub == "todo" or stub == "unimplemented" then risk("introduced " .. stub .. "! stub", node, scope) end
    elseif kind == "attribute_item" or kind == "inner_attribute_item" then
      local attribute = compact(syntax_text(node, source))
      local conditional = attribute:match("^#!?%[cfg_attr%(")
      local sibling = node:next_named_sibling()
      while sibling and (sibling:type() == "attribute_item" or sibling:type() == "line_comment" or sibling:type() == "block_comment") do
        sibling = sibling:next_named_sibling()
      end
      local owner = kind == "attribute_item" and sibling and declaration(sibling, source) or nil
      local attributed_scope = owner and scope .. "/" .. owner or scope
      if attribute:match("^#!?%[allow%(") or attribute:match("^#!?%[expect%(")
        or (conditional and (attribute:match(",allow%(") or attribute:match(",expect%("))) then
        risk("introduced lint suppression", node, attributed_scope)
      elseif attribute:match("^#%[ignore[%(%]=]") or (conditional and attribute:match(",ignore[%(%)]")) then
        risk("introduced test ignore", node, attributed_scope, conditional and attribute or "ignore")
      end
      if attribute == "#[test]" or attribute:match("^#%[[%w_:]*::test%]") or attribute:match("^#%[test_case%(")
        or (conditional and attribute:match(",[%w_:]*test%)")) then
        counted(result.tests, attributed_scope, node)
      end
    elseif kind == "decorator" then
      local decorator = compact(text(node, source))
      if decorator:match("^@pytest%.mark%.skip") or decorator:match("^@unittest%.skip") then
        local sibling = node:next_named_sibling()
        while sibling and sibling:type() == "decorator" do sibling = sibling:next_named_sibling() end
        local owner = sibling and declaration(sibling, source) or nil
        risk("introduced test skip", node, owner and scope .. "/" .. owner or scope)
      end
    elseif kind == "comment" or kind == "line_comment" or kind == "block_comment" then
      local comment = text(node, source)
      local directive = comment:gsub("^//+%s*", ""):gsub("^/%*+%s*", "")
      local script = language == "javascript" or language == "typescript" or language == "tsx"
      local identity
      if script then
        identity = directive:match("^(@ts%-ignore)%f[^%w_]") or directive:match("^(@ts%-nocheck)%f[^%w_]")
          or directive:match("^(@ts%-expect%-error)%f[^%w_]")
        if not identity and directive:match("^eslint%-disable%f[^%w_]") then
          identity = compact(directive:gsub("%-%-.*$", ""):gsub("%*/%s*$", ""))
        end
      elseif language == "python" then
        local lower = comment:lower()
        if lower:match("^#%s*noqa%f[^%w_]") then
          local codes = lower:match("^#%s*noqa%s*:%s*(.*)") or ""
          local selected = {}
          for code in codes:gmatch("[^,%s]+") do
            if not code:match("^%a+%d+$") then break end
            selected[#selected + 1] = code
          end
          identity = "noqa:" .. table.concat(selected, ",")
        elseif lower:match("^#%s*type:%s*ignore") then
          identity = lower:match("type:%s*ignore%s*(%[[^%]]*%])") or "type:ignore"
        end
      end
      if identity then risk("introduced suppression directive", node, scope, identity) end
    end
    if language == "python" and kind == "function_definition" then
      local name = field(node, "name", source) or ""
      if name:match("^test_") then counted(result.tests, scope, node) end
    end
    if identity or ANONYMOUS_FUNCTION[kind] then
      for _, slot in ipairs({ "type", "return_type" }) do
        local type_nodes = node:field(slot)
        if type_nodes[1] then
          local value = text(type_nodes[1], source)
          local key = scope .. "/" .. slot
          if result.types[key] then error("ambiguous typed declaration: " .. key) end
          result.types[key] = { value = value, wide = is_wide(syntax_text(type_nodes[1], source), imported, language), line = line(type_nodes[1]) }
        end
      end
    end
    for _, child in ipairs(node:named_children()) do walk(child, scope, depth + 1) end
  end
  walk(root, "<file>", 0)
  return result
end

local function new_region(entry, unchanged)
  for _, range in ipairs(unchanged) do
    if entry.start >= range[2] and entry.finish <= range[2] + range[3] then return false end
  end
  return true
end

local function explicit_removal(request, kind, path, symbol)
  if type(request) ~= "string" or #request > 65536 then return false end
  request = request:gsub("```.-```", ""):gsub("~~~.-~~~", "")
  local pending = math.min(request:find("```", 1, true) or (#request + 1), request:find("~~~", 1, true) or (#request + 1))
  request = request:sub(1, pending - 1)
  request = request:gsub("%.%s+", "\n"):gsub("%.$", "")
  local target = kind == "removed assertion" and { assertion = true, assertions = true } or { test = true, tests = true }
  local adjectives = { the = true, obsolete = true, old = true, redundant = true, existing = true,
    all = true, these = true, those = true, unnecessary = true, outdated = true,
    unit = true, integration = true, regression = true }
  local function subject(body)
    for _ = 1, 6 do
      local first, rest = body:match("^(%a+)%s+(.*)$")
      if not first or not adjectives[first] then break end
      body = rest
    end
    local noun, tail = body:match("^(%a+)(.*)$")
    return noun, tail or ""
  end
  local function named(selector)
    selector = selector:gsub("^[`'\"]", ""):gsub("[`'\"]$", ""):gsub("%(%)$", "")
    if selector == path:lower() then return true end
    local names = {}
    for part in symbol:gmatch("[^/]+") do
      local name = part:match("^[%w_]+:(.*)$")
      if name and not name:match("^<") then
        names[#names + 1] = name:gsub("^[`'\"]", ""):gsub("[`'\"]$", ""):lower()
      end
    end
    local qualified = table.concat(names, "::")
    return qualified == selector or (kind == "removed assertion" and qualified:sub(1, #selector + 2) == selector .. "::")
  end
  local function requested(noun, tail)
    if not target[noun] then return false end
    local qualifier = tail:match("^%s+in%s+([^%s]+)") or tail:match("^%s+from%s+([^%s]+)")
    if qualifier then return named(qualifier) end
    local selector = tail:match("^%s+([^%s]+)")
    local connective = { ["and"] = true, because = true, that = true, which = true,
      without = true, with = true, ["for"] = true }
    return not selector or connective[selector] or named(selector)
  end
  -- Only an affirmative imperative at a sentence/list boundary. Mentioning
  -- tests, asking to skip a build, or quoting source does not grant a waiver.
  for sentence in ("\n" .. request:lower()):gmatch("[\n!?]%s*([^\n!?]+)") do
    sentence = sentence:gsub("^[%-%*]%s*", ""):gsub("^please%s+", "")
    local body = sentence:match("^remove%s+(.+)") or sentence:match("^delete%s+(.+)")
      or sentence:match("^comment%s+out%s+(.+)")
    if body then
      local noun, tail = subject(body)
      if requested(noun, tail) then return true end
      if noun == "test" or noun == "tests" or noun == "assertion" or noun == "assertions" then
        local other = tail:match("^%s+and%s+(.+)")
        if other then
          local second, rest = subject(other)
          if requested(second, rest) then return true end
        end
      end
    end
  end
  return false
end

local function collect(snapshot, request, work)
  if type(snapshot) ~= "table" or type(snapshot.mutated) ~= "boolean" then return nil, "missing or malformed frozen diff snapshot" end
  if not snapshot.mutated then return {}, digest({ "no mutations" }) end
  if snapshot.paths ~= nil and snapshot.paths ~= maki.NIL then
    if type(snapshot.paths) ~= "table" then return nil, "malformed owned-path scope" end
    local encoded = maki.json.encode(snapshot.paths)
    if not encoded or encoded:sub(1, 1) ~= "[" then return nil, "malformed owned-path array" end
    local count = 0
    for key, path in pairs(snapshot.paths) do
      if type(key) ~= "number" or key < 1 or key % 1 ~= 0 or type(path) ~= "string" then
        return nil, "malformed owned-path array"
      end
      count = count + 1
    end
    if count ~= #snapshot.paths then return nil, "non-contiguous owned-path array" end
    if #snapshot.paths == 0 then return {}, digest({ "empty task scope" }) end
  end
  if snapshot.captured ~= true or type(snapshot.root) ~= "string" or snapshot.root == ""
    or snapshot.root:find("[%z\r\n]")
    or type(snapshot.reference) ~= "string" or not snapshot.reference:match("^%x+$")
    or (#snapshot.reference ~= 40 and #snapshot.reference ~= 64)
    or type(snapshot.attributes) ~= "string" or not snapshot.attributes:match("^%x+$")
    or (#snapshot.attributes ~= 40 and #snapshot.attributes ~= 64) then
    return nil, "mutations occurred without a captured Git turn baseline; integrity cannot be verified"
  end
  local top, err = git(snapshot, { "rev-parse", "--show-toplevel" }, 4096)
  if not top then return nil, err end
  if path_key(top) ~= path_key(snapshot.root) then return nil, "Git root does not match the frozen snapshot root" end
  local output
  output, err = git(snapshot, { "diff", "--no-ext-diff", "--no-renames", "--name-only", "-z", snapshot.reference, "--" }, MAX_GIT)
  if not output then return nil, err end
  local changed
  changed, err = paths(output)
  if not changed then return nil, err end
  output, err = git(snapshot, { "ls-files", "--others", "--exclude-standard", "-z" }, MAX_GIT)
  if not output then return nil, err end
  local untracked
  untracked, err = paths(output)
  if not untracked then return nil, err end
  output, err = git(snapshot, { "ls-tree", "-r", "--name-only", "-z", snapshot.reference }, MAX_GIT)
  if not output then return nil, err end
  local baseline_paths
  baseline_paths, err = paths(output)
  if not baseline_paths then return nil, err end
  output, err = git(snapshot, { "diff", "--no-ext-diff", "--find-renames", "--name-status", "-z", snapshot.reference, "--" }, MAX_GIT)
  if not output then return nil, err end
  local status = {}
  for token in output:gmatch("([^%z]+)%z") do status[#status + 1] = token end
  local renamed, old_names, position = {}, {}, 1
  while position <= #status do
    local code, old = status[position], status[position + 1]
    if not old then return nil, "malformed Git change status" end
    if code:match("^R%d+$") then
      local new = status[position + 2]
      if not new then return nil, "malformed Git rename status" end
      renamed[new], old_names[old] = old, new
      position = position + 3
    else
      position = position + 2
    end
  end
  local baseline, selected, owned = {}, {}, nil
  for _, path in ipairs(baseline_paths) do baseline[path] = true end
  if type(snapshot.paths) == "table" then
    owned = {}
    for _, path in ipairs(snapshot.paths) do
      if type(path) ~= "string" then return nil, "malformed owned path" end
      owned[path_key(path)] = true
    end
  end
  for _, list in ipairs({ changed, untracked }) do
    for _, path in ipairs(list) do
      if not owned or owned[path_key(maki.fs.joinpath(snapshot.root, path))] then selected[path] = true end
    end
  end
  local ordered = {}
  for path in pairs(selected) do
    local language = maki.treesitter.language_for_path(path)
    local original_language = renamed[path] and maki.treesitter.language_for_path(renamed[path]) or nil
    if (SUPPORTED[language] or SUPPORTED[original_language])
      and (not old_names[path] or not selected[old_names[path]]) then
      ordered[#ordered + 1] = path
    end
  end
  table.sort(ordered)
  if #ordered > MAX_FILES then return nil, "changed-file analysis budget exceeded (256 files)" end
  local findings, fingerprint, total = {}, { snapshot.reference }, 0
  local function finding(kind, path, symbol, row, evidence, removed_test_authorized)
    if #findings == MAX_FINDINGS then error("integrity finding budget exceeded") end
    if removed_test_authorized or ((kind == "removed assertion" or kind == "removed test")
      and explicit_removal(request, kind, path, symbol)) then return end
    findings[#findings + 1] = { kind = kind, path = path, symbol = symbol, line = row, evidence = evidence }
  end
  output, err = git(snapshot, { "diff", "--no-ext-diff", "--no-renames", "--numstat", "-z", snapshot.reference, "--" }, MAX_GIT)
  if not output then return nil, err end
  local binary = {}
  for record in output:gmatch("([^%z]+)%z") do
    local removed, introduced, path = record:match("^([^\t]+)\t([^\t]+)\t(.*)$")
    if not path then return nil, "malformed Git binary classification" end
    if removed == "-" or introduced == "-" then binary[path] = true end
  end
  local units = {}
  for _, path in ipairs(ordered) do
    local before, after = "", ""
    local before_path = renamed[path] or path
    if baseline[before_path] then
      local size_output
      size_output, err = git(snapshot, { "cat-file", "-s", snapshot.reference .. ":" .. before_path }, 32)
      if not size_output then return nil, err end
      local size = tonumber(size_output)
      if not size or size > MAX_SOURCE then return nil, "baseline source byte budget exceeded: " .. before_path end
      before, err = git(snapshot, { "show", "--no-ext-diff", snapshot.reference .. ":" .. before_path }, MAX_SOURCE + 1)
      if not before then return nil, err end
    end
    local absolute = maki.fs.joinpath(snapshot.root, path)
    local metadata
    metadata, err = maki.fs.metadata(absolute)
    if err then return nil, "cannot stat changed file " .. path .. ": " .. tostring(err) end
    if metadata then
      local bytes
      bytes, err = maki.fs.read_bytes(absolute, { offset = 0, len = MAX_SOURCE + 1 })
      if not bytes then return nil, "cannot read changed file " .. path .. ": " .. tostring(err) end
      after = buffer.tostring(bytes)
    end
    if #before > MAX_SOURCE or #after > MAX_SOURCE then return nil, "source analysis budget exceeded: " .. path end
    total = total + #before + #after
    if total > MAX_TOTAL then return nil, "total source analysis budget exceeded (4 MiB)" end
    before, after = normalize(before), normalize(after)
    fingerprint[#fingerprint + 1] = path
    fingerprint[#fingerprint + 1] = before_path
    fingerprint[#fingerprint + 1] = before
    fingerprint[#fingerprint + 1] = after
    work[#work + 1] = path
    work[#work + 1] = before
    work[#work + 1] = after
    if binary[path] or binary[before_path] then return nil, "changed code is binary: " .. path end
    local old_language = maki.treesitter.language_for_path(before_path)
    local new_language = maki.treesitter.language_for_path(path)
    if SUPPORTED[old_language] and not SUPPORTED[new_language] then
      return nil, "code moved to an unsupported/resource path: " .. path
    end
    units[#units + 1] = { path = path, before_path = before_path, before = before, after = after }
  end
  -- Git cannot pair a deleted tracked file with an unstaged, untracked
  -- destination. Pair those by source similarity with a separate hard work
  -- cap, then compare their declarations rather than two unrelated file totals.
  local deleted, added = {}, {}
  for _, unit in ipairs(units) do
    if unit.before ~= "" and unit.after == "" then deleted[#deleted + 1] = unit end
    if unit.before == "" and unit.after ~= "" then added[#added + 1] = unit end
  end
  local comparisons, candidates = 0, {}
  for _, old in ipairs(deleted) do
    for _, new in ipairs(added) do
      if maki.treesitter.language_for_path(old.path) == maki.treesitter.language_for_path(new.path) then
        comparisons = comparisons + 1
        if comparisons > 32 then return nil, "unstaged rename analysis budget exceeded (32 comparisons)" end
        local equal
        equal, err = maki.text.unchanged_ranges(old.before, new.after)
        if not equal then return nil, "cannot analyze unstaged rename: " .. tostring(err) end
        local length = 0
        for _, range in ipairs(equal) do length = length + range[3] end
        local score = length / math.max(#old.before, #new.after)
        if score >= 0.5 then candidates[#candidates + 1] = { old = old, new = new, score = score } end
      end
    end
  end
  table.sort(candidates, function(a, b)
    if a.score ~= b.score then return a.score > b.score end
    if a.old.path ~= b.old.path then return a.old.path < b.old.path end
    return a.new.path < b.new.path
  end)
  for _, pair in ipairs(candidates) do
    if not pair.old.paired and not pair.new.paired then
      pair.new.before, pair.new.before_path = pair.old.before, pair.old.path
      pair.new.paired, pair.old.paired = true, true
      pair.old.skip = true
    end
  end
  for _, unit in ipairs(units) do
    local path, before, after = unit.path, unit.before, unit.after
    if not unit.skip and before ~= after then
      local old, current
      old, err = analyze(unit.before_path, before)
      if not old then return nil, err end
      current, err = analyze(path, after)
      if not current then return nil, err end
      for _, key in ipairs({ "assertions", "tests" }) do
        for symbol, entry in pairs(old[key]) do
          local remaining = current[key][symbol] and current[key][symbol].count or 0
          if remaining < entry.count then
            local removed_test_authorized = false
            if key == "assertions" then
              for test in pairs(old.tests) do
                if not current.tests[test] and (symbol == test or symbol:sub(1, #test + 1) == test .. "/")
                  and explicit_removal(request, "removed test", path, test) then
                  removed_test_authorized = true
                  break
                end
              end
            end
            finding(key == "tests" and "removed test" or "removed assertion", path, symbol, entry.line,
              tostring(entry.count - remaining) .. " net AST removal(s)", removed_test_authorized)
          end
        end
      end
      local unchanged
      unchanged, err = maki.text.unchanged_ranges(before, after)
      if not unchanged then return nil, "cannot compare changed regions: " .. tostring(err) end
      for _, entry in ipairs(current.risks) do
        local matched = false
        for index, previous in ipairs(old.risks) do
          if previous and previous.kind == entry.kind and previous.symbol == entry.symbol
            and previous.identity == entry.identity then
            old.risks[index] = false
            matched = true
            break
          end
        end
        local rebound = false
        if not matched then
          for _, previous in ipairs(old.risks) do
            if previous and previous.kind == entry.kind and previous.identity == entry.identity
              and previous.symbol ~= entry.symbol then rebound = true; break end
          end
        end
        if not matched and (rebound or new_region(entry, unchanged)) then
          finding(entry.kind, path, entry.symbol, entry.line, entry.evidence)
        end
      end
      for symbol, entry in pairs(current.types) do
        local previous = old.types[symbol]
        if previous and not previous.wide and entry.wide then
          finding("strict type widened", path, symbol, entry.line, previous.value .. " -> " .. entry.value)
        end
      end
    end
  end
  table.sort(findings, function(a, b)
    if a.path ~= b.path then return a.path < b.path end
    if a.line ~= b.line then return a.line < b.line end
    if a.kind ~= b.kind then return a.kind < b.kind end
    return a.symbol < b.symbol
  end)
  return findings, digest(fingerprint)
end

local function justified(reply, identifier, entry)
  if type(reply) ~= "string" or #reply > 65536 then return false end
  -- A warning renders findings as "<identifier> <path>:<line>", so accept that
  -- echoed location between the identifier and the rationale.
  local location = entry.path .. ":" .. entry.line
  for body in reply:gmatch("%[DIFF INTEGRITY JUSTIFICATION%]([^\n]+)") do
    local id, rest = body:match("^%s*([^%s:]+)%s*(.-)%s*$")
    if id == identifier then
      local separator = "%s*:%s*"
      if rest:sub(1, #location) == location then rest, separator = rest:sub(#location + 1), "%s*:?%s*" end
      local rationale = rest:match("^" .. separator .. "(.-)%s*$")
      if rationale and #rationale >= 40 and #rationale <= 2000 then
        local lower = rationale:lower()
        local words = 0
        for _ in rationale:gmatch("%S+") do words = words + 1 end
        local concrete = rationale:find(entry.path, 1, true) or rationale:find(entry.symbol, 1, true)
        -- Analysis-unavailable findings carry a synthetic path, so a rationale may
        -- instead name a real file path mentioned in the evidence.
        if not concrete and entry.path == "<integrity>" then
          for token in entry.evidence:gmatch("%S+") do
            token = token:match("^%p*(.-)%p*$")
            if #token >= 4 and token:find(".", 1, true) and rationale:find(token, 1, true) then
              concrete = true
              break
            end
          end
        end
        local reason = lower:match("%f[%a]because%f[^%a]") or lower:match("%f[%a]replaced%f[^%a]")
          or lower:match("%f[%a]required%f[^%a]") or lower:match("%f[%a]intentional%f[^%a]")
        local generic = { this = true, is = true, the = true, a = true, an = true, ["and"] = true, ["or"] = true,
          because = true, change = true, changes = true, intentional = true, required = true, necessary = true,
          justified = true, safe = true, correct = true, appropriate = true, ["for"] = true, it = true, to = true,
          be = true, was = true, as = true, i = true, have = true, done = true, requested = true, user = true }
        local substantive, seen = 0, {}
        for word in lower:gmatch("[%a_][%w_]*") do
          if #word >= 4 and not generic[word] and not seen[word] and not entry.path:lower():find(word, 1, true) then
            seen[word] = true
            substantive = substantive + 1
          end
        end
        if words >= 8 and substantive >= 3 and concrete and reason and not lower:match("no reason") then
          return true
        end
      end
    end
  end
  return false
end

function M.gate(value, previous)
  local work = { maki.json.encode(value.diff_snapshot) or "malformed" }
  local ok, findings, fingerprint = pcall(collect, value.diff_snapshot, value.user_request, work)
  if not ok or not findings then
    local reason = tostring(ok and fingerprint or findings)
    findings = { { kind = "analysis unavailable", path = "<integrity>", symbol = "Git baseline analysis",
      line = 0, evidence = reason } }
    work[#work + 1] = reason
    fingerprint = digest(work)
  end
  local unresolved = {}
  for _, entry in ipairs(findings) do
    local identifier = "DI-" .. fingerprint .. "-" .. digest({ entry.kind, entry.path, entry.symbol,
      tostring(entry.line), entry.evidence })
    if not justified(value.last_message, identifier, entry) then
      unresolved[#unresolved + 1] = identifier .. " " .. entry.path .. ":" .. entry.line
        .. " " .. entry.kind .. " in " .. entry.symbol .. " — " .. entry.evidence
    end
  end
  if #unresolved == 0 then return previous end
  local prompt = WARNING .. "\n" .. table.concat(unresolved, "\n")
    .. "\nFix each finding or add one current-reply line per finding: " .. MARKER
    .. " <exact DI identifier>: <concrete rationale naming its path/symbol and explaining why>."
  if type(previous) == "table" and type(previous.continue) == "string" and previous.continue ~= "" then
    prompt = previous.continue .. "\n\n" .. prompt
  end
  return { continue = prompt, required = true }
end

return M
