-- The path completion popup: the rows it shows, the keys it takes while it is
-- up, and the edit that accepts a row. Mirrors the `@` completion popup — an
-- unfocused float on the input caret — so the user keeps typing into the chat
-- input beneath it.

local ListPicker = require("maki.list_picker")
local Skills = require("maki.skills")
local Mcp = require("tc.mcp")
local Path = require("tc.path")

local M = {}

local opts = maki.api.register_options({
  max_items = { default = 12, min = 1, desc = "Rows the path completion popup shows at once." },
})

local FOOTER = { { "↑/↓", "move" }, { "Tab", "insert" }, { "Esc", "close" } }
local ZINDEX = 120
local BORDER = 2
local PAD = 2
local NO_MATCHES = "no matches"
local TEARDOWN_FAILED = "tabcomplete: teardown step failed: "
local EDIT_REFUSED = "tabcomplete: the path was not inserted: "

-- `{ win, buf, sel, view }`. A view is what one refresh drew: the rows, and the
-- input snapshot they answer, which is what the accept writes over.
local popup = nil
-- The session's working tree, read once per popup: a second session opened
-- elsewhere must not offer files from the wrong tree.
local cwd = nil

local function tree_of(session_id)
  if cwd and cwd.session_id == session_id then
    return cwd.root
  end
  local session = maki.session.read()
  if not session or not session.cwd then
    return nil
  end
  cwd = { session_id = session_id, root = session.cwd }
  return cwd.root
end

function M.session_id()
  return popup and popup.view.session_id
end

local function lines()
  local view = popup.view
  local out = {}
  if #view.rows == 0 then
    out[1] = { { " " .. NO_MATCHES, "dim" } }
    return out
  end
  for i, item in ipairs(view.rows) do
    local selected = i == popup.sel
    local base = selected and "selected" or "item"
    local matched = selected and "match_selected" or "match"
    out[i] = ListPicker.range_spans(item.label, item.highlights, base, matched)
    table.insert(out[i], 1, { " ", base })
  end
  return out
end

local function render(size)
  local view = popup.view
  local width = 0
  for _, item in ipairs(view.rows) do
    width = math.max(width, maki.ui.display_width(item.label) + PAD + BORDER)
  end
  if #view.rows == 0 then
    width = maki.ui.display_width(NO_MATCHES) + PAD + BORDER
  end
  popup.buf:set_lines(lines())
  popup.win:set_config({ width = math.min(width, size.cols), height = math.max(1, #view.rows) + BORDER })
  popup.win:show()
end

local function move(delta)
  if not popup then
    return
  end
  local n = #popup.view.rows
  if n > 0 then
    popup.sel = (popup.sel - 1 + delta) % n + 1
    popup.buf:set_lines(lines())
  end
end

local function next_row()
  move(1)
end

local function prev_row()
  move(-1)
end

-- Insert the highlighted row over the token it was listed for. The range, the
-- version and the session all came with the snapshot the rows answer, and
-- `input_edit` weighs that snapshot itself and refuses an edit the input has
-- moved past. The popup closes first, handing the keys back before the edit
-- fires the `InputChanged` this plugin listens to.
function M.accept()
  local view = popup and popup.view
  local choice = view and view.rows[popup.sel]
  if not choice then
    return M.close()
  end
  M.close()
  local ok, err = maki.ui.input_edit({
    start = view.start,
    stop = view.cursor,
    text = choice.insert,
    version = view.version,
    session_id = view.session_id,
  })
  if not ok then
    maki.ui.flash(EDIT_REFUSED .. tostring(err))
  end
end

function M.close()
  if not popup then
    return
  end
  local mine = popup
  popup = nil
  local ok, err = pcall(function()
    mine.win:close()
  end)
  if not ok then
    maki.log.warn(TEARDOWN_FAILED .. tostring(err))
  end
end

-- The keys the popup takes while it is on screen. `<Tab>` is the whole point:
-- while the popup is up it accepts the row, and the host routes it here before
-- the chat input's mode toggle, so the toggle is only reached with no popup.
M.BINDINGS = {
  { claim = "<Down>", run = next_row },
  { claim = "<Up>", run = prev_row },
  { claim = "<C-n>", run = next_row },
  { claim = "<C-p>", run = prev_row },
  { claim = "<Esc>", run = M.close },
  { claim = "<Tab>", run = M.accept },
  { claim = "<CR>", run = M.accept },
}

local KEYS, HANDLERS = {}, {}
for i, binding in ipairs(M.BINDINGS) do
  KEYS[i] = binding.claim
  HANDLERS[binding.claim] = binding.run
end

function M.handle_key(key)
  local handler = HANDLERS[key]
  if handler then
    handler()
  end
end

local function read_keys(mine)
  maki.async.run(function()
    while true do
      local ev = mine.win:recv()
      if not ev or ev.type == "close" then
        return
      end
      if ev.type == "key" and popup == mine then
        M.handle_key(ev.key)
      end
    end
  end)
end

local function ensure_popup()
  if popup then
    return
  end
  local buf = maki.ui.buf()
  local win = maki.ui.open_win(buf, {
    width = PAD + BORDER,
    height = BORDER + 1,
    anchor = "input_caret",
    border = "rounded",
    footer = FOOTER,
    zindex = ZINDEX,
    focus = false,
    visible = false,
    keys = KEYS,
  })
  popup = {
    win = win,
    buf = buf,
    sel = 1,
  }
  read_keys(popup)
end

-- Lists the directory the token names, the skills a `/skill:` token names, or
-- the servers a `/mcp auth` token names, and paints the rows. A caret that
-- has left the token (no longer a path, a skill mention or a command
-- argument) takes the popup down: the rows on screen answer a token the
-- user is no longer typing.
function M.refresh(st)
  local start, token = Path.parse(st.text, st.cursor)
  local rows
  local skill_prefix = Path.skill_prefix(token)
  if skill_prefix then
    rows = Skills.rows(skill_prefix, opts.max_items)
  elseif Mcp.is_auth_arg(st.text, start) then
    rows = Mcp.rows(token, opts.max_items)
  elseif Path.is_path(token) then
    local root = tree_of(st.session_id)
    if not root then
      return M.close()
    end
    local resolved = Path.resolve(token, root, maki.fs.abspath("~"))
    local dir = maki.fs.normalize(resolved.dir)
    local raw = maki.fs.dir(dir, { depth = 1 })
    rows = raw and Path.entries(raw, resolved.disp, resolved.prefix, opts.max_items) or {}
  else
    return M.close()
  end
  ensure_popup()
  local was = popup.view and popup.view.rows[popup.sel]
  popup.view = {
    rows = rows,
    start = start,
    cursor = st.cursor,
    version = st.version,
    session_id = st.session_id,
  }
  popup.sel = 1
  for i, item in ipairs(rows) do
    if was and item.insert == was.insert then
      popup.sel = i
    end
  end
  render(maki.ui.terminal_size())
end

-- Hands a refresh off, so autocmd dispatch and the key handler do not wait on
-- the directory read.
function M.refresh_later(st)
  maki.async.run(function()
    M.refresh(st)
  end)
end

return M
