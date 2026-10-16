local SKILL_FILE = "SKILL.md"
local NOT_FOUND = "skill not found: "
local REFERENCE_FILE = "lua-api.md"
local REFERENCE_UNAVAILABLE = "(unavailable; full reference inlined below)"
local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")
local ListPicker = require("maki.list_picker")
local Skills = require("maki.skills")
local helpers = require("skill_helpers")
local store = require("skill_store")
local parse_frontmatter = helpers.parse_frontmatter
local build_skill_list = helpers.build_skill_list
local format_output = helpers.format_output
local parse_output = helpers.parse_output
local sorted = helpers.sorted
local toggle_disabled = helpers.toggle_disabled

local PROJECT_SKILL_DIRS = {
  ".maki/skills",
  ".claude/skills",
  ".opencode/skills",
  ".agents/skills",
  ".agent/skills",
  ".github/skills",
}
local GLOBAL_SKILL_DIRS = {
  ".claude/skills",
  ".config/opencode/skills",
  ".agents/skills",
  ".copilot/skills",
}

local function scan_skill_dir(dir, skills)
  local entries = maki.fs.dir(dir)
  if not entries then
    return
  end
  for _, entry in ipairs(entries) do
    if entry[2] == "directory" then
      local skill_path = maki.fs.joinpath(dir, entry[1], SKILL_FILE)
      local content = maki.fs.read(skill_path)
      if content then
        local fm, body = parse_frontmatter(content)
        if body and #body > 0 then
          local name = (fm and fm.name) or entry[1]
          skills[name] = {
            name = name,
            description = (fm and fm.description) or "",
            content = body,
            location = skill_path,
          }
        end
      end
    end
  end
end

local function find_project_ancestors()
  local cwd = maki.uv.cwd()
  if not cwd then
    return {}
  end
  local dirs = { cwd }
  if maki.fs.metadata(maki.fs.joinpath(cwd, ".git")) then
    return dirs
  end
  for _, parent in ipairs(maki.fs.parents(cwd)) do
    dirs[#dirs + 1] = parent
    if maki.fs.metadata(maki.fs.joinpath(parent, ".git")) then
      break
    end
  end
  return dirs
end

local opts = maki.api.register_options({
  plugin_dev = { default = true, desc = "Offer the builtin maki-plugin-dev skill for writing maki plugins." },
})

local ok, builtin, reference = pcall(function()
  return require("plugin_dev"), require("plugin_dev_reference")
end)
if not ok then
  maki.log.warn("builtin plugin_dev skill unavailable: " .. tostring(builtin))
  builtin = nil
end

local function resolve_builtin_content()
  local state = maki.env.state_dir()
  if state then
    local dir = maki.fs.joinpath(state, "docs")
    local path = maki.fs.joinpath(dir, REFERENCE_FILE)
    local _, err = maki.fs.mkdir(dir, { parents = true })
    if not err then
      _, err = maki.fs.write(path, reference.content)
    end
    if not err then
      return (builtin.content:gsub(builtin.reference_placeholder, function()
        return path
      end))
    end
    maki.log.warn("failed to write lua api reference to " .. path .. ": " .. tostring(err))
  end
  local content = builtin.content:gsub(builtin.reference_placeholder, REFERENCE_UNAVAILABLE)
  return content .. "\n---\n\n" .. reference.content
end

local disabled = store.load()
local cached_skills = {}

Skills.publish(function()
  return cached_skills
end)

local function discover_skills()
  local skills = {}

  if builtin and opts.plugin_dev then
    skills[builtin.name] = {
      name = builtin.name,
      description = builtin.description,
      content = builtin.content,
      location = "builtin:" .. builtin.name,
      resolve = resolve_builtin_content,
    }
  end

  local config = maki.env.config_dir()
  if config then
    scan_skill_dir(maki.fs.joinpath(config, "skills"), skills)
  end

  local home = maki.uv.os_homedir()
  if home then
    for _, rel in ipairs(GLOBAL_SKILL_DIRS) do
      scan_skill_dir(maki.fs.joinpath(home, rel), skills)
    end
  end

  for _, ancestor in ipairs(find_project_ancestors()) do
    for _, rel in ipairs(PROJECT_SKILL_DIRS) do
      scan_skill_dir(maki.fs.joinpath(ancestor, rel), skills)
    end
  end

  for _, skill in pairs(skills) do
    skill.enabled = not disabled[skill.name]
  end
  cached_skills = sorted(skills)
  return skills
end

local function view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 20, keep = "head" }
end

-- Live call and restore both draw from the llm text, so the two views
-- cannot drift. Nil when the text is not a skill listing.
local function skill_view(output, ctx)
  local location, content = parse_output(output)
  if not location then
    return nil
  end

  local buf = maki.ui.buf()
  local view = ToolView.new(buf, view_opts(ctx))
  buf:on("click", function()
    view:toggle()
  end)
  view:set_highlight(content, location:match("%.([^%.]+)$") or "md")
  view:finish()

  local header = maki.ui.buf()
  header:line({ { shorten_path(location), "path" } })
  return { body = buf, header = header }
end

local boot_skills = discover_skills()
local description = "Load a skill that provides instructions and workflows for specific tasks."
  .. build_skill_list(boot_skills)

maki.api.register_tool({
  name = "skill",
  kind = "read",
  description = description,

  schema = {
    type = "object",
    properties = {
      name = { type = "string", description = "Name of the skill to load", required = true },
    },
  },

  header = function(input)
    return input.name
  end,

  restore = function(_input, output, _is_error, ctx)
    return skill_view(output, ctx) or ToolView.restore(output, view_opts(ctx))
  end,

  handler = function(input, ctx)
    if not input.name then
      return { llm_output = "error: name is required", is_error = true }
    end

    local skills = discover_skills()
    local skill = skills[input.name]
    if not skill or skill.enabled == false then
      local available = build_skill_list(skills)
      return { llm_output = NOT_FOUND .. input.name .. available, is_error = true }
    end
    if skill.resolve then
      skill.content = skill.resolve()
    end

    local llm_output = format_output(skill.location, skill.content)
    local result = skill_view(llm_output, ctx)
    result.llm_output = llm_output
    return result
  end,
})

-- ── /skills picker ──

local PICKER_FOOTER = { { "Enter", "details" }, { "d", "disable" }, { "Esc", "close" } }
local DISABLED_LABEL = "disabled"
local NO_SKILLS = "No skills found."

-- Rows for the picker: name, then description · location · enabled state. The
-- detail is a list of styled parts so a narrow row cuts the description and
-- keeps the location whole.
local function picker_items(skills)
  local items = {}
  for _, s in ipairs(sorted(skills)) do
    local state = s.enabled == false and DISABLED_LABEL or shorten_path(s.location)
    local state_style = s.enabled == false and "error" or "dim"
    items[#items + 1] = {
      label = s.name,
      detail = {
        { s.description, "dim", elastic = true },
        { " · " .. state, state_style },
      },
      skill = s,
    }
  end
  return items
end

-- Shows the skill body in a focused window, with its location in the title.
local function view_details(skill)
  if skill.resolve then
    skill.content = skill.resolve()
  end
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, {
    max_lines = math.huge,
    max_expand_lines = math.huge,
    keep = "head",
  })
  local ext = skill.location:match("%.([^%.]+)$") or "md"
  if not view:set_highlight(skill.content, ext) then
    for line in skill.content:gmatch("([^\n]*)\n?") do
      view:append(line)
    end
  end
  view:finish()
  local win = maki.ui.open_win(buf, {
    title = " " .. skill.name .. " ",
    width = "80%",
    height = "80%",
    border = "rounded",
    focus = true,
    footer = { { "Esc", "close" } },
  })
  while true do
    local ev = win:recv()
    if not ev or ev.type == "close" then
      return
    end
    if ev.type == "key" and (ev.key == "<Esc>" or ev.key == "<C-c>") then
      win:close()
      return
    end
  end
end

local function open_picker()
  local skills = discover_skills()
  if next(skills) == nil then
    maki.ui.flash(NO_SKILLS)
    return
  end
  local event = ListPicker.open(picker_items(skills), {
    title = " Skills ",
    footer = PICKER_FOOTER,
    key = function(item)
      return item.label
    end,
    live_keys = {
      d = function(item)
        if not item then
          return nil
        end
        toggle_disabled(disabled, item.skill.name)
        item.skill.enabled = not disabled[item.skill.name]
        return picker_items(skills)
      end,
    },
  })
  local err = store.save(disabled)
  if err then
    maki.ui.flash("could not save disabled skills: " .. tostring(err))
  end
  cached_skills = sorted(skills)
  if event.type == "choice" and event.item then
    view_details(event.item.skill)
  end
end

maki.api.register_command({
  name = "/skills",
  description = "Browse skills, view details, and disable them",
  handler = open_picker,
})
