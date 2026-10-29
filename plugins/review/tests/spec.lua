local FILE = "sample.rs"
local COMMENT = "Fix this branch\nKeep the existing behavior"
local SUBMISSION_ERROR = "session creation refused"
local FIX_SESSION = "review-fix-session"
local DIFF_LINES = 3200
local INPUT_WIDTH = 120
local INPUT_HEIGHT = 42
local SYNTAX_FG = "#abcdef"
local RENDER_ERROR = "review render failed"
local TYPED_COMMENT = "Fix the native smoke review issue on this line."

local function eq(actual, expected, message)
  assert(
    actual == expected,
    (message or "unexpected value") .. ": " .. tostring(actual) .. " ~= " .. tostring(expected)
  )
end

local function has(text, part)
  assert(text:find(part, 1, true), "missing " .. part .. " in " .. text)
end

local function line_text(line)
  local parts = {}
  for _, span in ipairs(line) do
    parts[#parts + 1] = span[1]
  end
  return table.concat(parts)
end

local function review()
  local h = {
    buffers = {},
    windows = {},
    latest = {},
    events = {},
    flashes = {},
    submissions = {},
    highlights = 0,
    cols = INPUT_WIDTH,
    rows = INPUT_HEIGHT,
  }
  local raw = { "@@ -0,0 +1," .. DIFF_LINES .. " @@" }
  for i = 1, DIFF_LINES do
    raw[#raw + 1] = "+let value_" .. i .. " = " .. i .. ";"
  end
  raw = table.concat(raw, "\n") .. "\n"

  local function output(cmd)
    if cmd == "git rev-parse --is-inside-work-tree" then
      return "true\n"
    elseif cmd == "git diff --no-color --name-status HEAD" then
      return "A\t" .. FILE .. "\n"
    elseif cmd == "git diff --no-color --numstat HEAD" then
      return DIFF_LINES .. "\t0\t" .. FILE .. "\n"
    elseif cmd == "git ls-files --others --exclude-standard" then
      return ""
    elseif cmd:find("git log", 1, true) then
      return "abc123\tExample commit\t1 day ago\n"
    elseif cmd:find("git diff --no-color HEAD --", 1, true) then
      return raw
    end
    error("unexpected git command: " .. cmd)
  end

  local api = {
    fn = {
      jobstart = function(cmd)
        return cmd
      end,
      jobwait = function(cmd)
        return { exit_code = 0, stdout = output(cmd) }
      end,
    },
    ui = {
      terminal_size = function()
        return { cols = h.cols, rows = h.rows }
      end,
      theme_color = function()
        return "#101010"
      end,
      flash = function(text)
        h.flashes[#h.flashes + 1] = text
      end,
      buf = function()
        local buf = { lines = {}, writes = 0, id = #h.buffers + 1 }
        function buf:set_lines(lines)
          if h.render_failure and self.id == 4 then
            error(RENDER_ERROR)
          end
          self.lines = lines
          self.writes = self.writes + 1
        end
        function buf:len()
          return #self.lines
        end
        h.buffers[#h.buffers + 1] = buf
        return buf
      end,
      highlight = function(code)
        h.highlights = h.highlights + 1
        local lines = {}
        for text in (code .. "\n"):gmatch("(.-)\n") do
          lines[#lines + 1] = { { text, { fg = SYNTAX_FG } } }
        end
        return lines
      end,
      open_win = function(buf, config)
        local win = {
          buf = buf,
          config = config,
          cursor = 1,
          closed = false,
          width = config.width - 2,
          height = config.height - 2,
        }
        function win:set_config(patch)
          for key, value in pairs(patch) do
            self.config[key] = value
          end
        end
        function win:set_cursor(row)
          self.cursor = row
        end
        function win:close()
          self.closed = true
        end
        function win:recv()
          local event = assert(table.remove(h.events, 1), "review stopped receiving events")
          while type(event) == "function" do
            event(h)
            event = assert(table.remove(h.events, 1), "missing event after assertion")
          end
          if type(event) == "string" then
            return { type = "key", key = event }
          end
          return event
        end
        h.windows[#h.windows + 1] = win
        h.latest[buf.id] = win
        return win
      end,
    },
    session = {
      new = function(opts)
        h.submissions[#h.submissions + 1] = opts
        if h.failure == "raised" then
          error(SUBMISSION_ERROR)
        end
        if h.failure == "returned" then
          return nil, SUBMISSION_ERROR
        end
        h.focused = FIX_SESSION
        return FIX_SESSION
      end,
    },
    api = {
      register_command = function(opts)
        h.open = opts.handler
      end,
      create_autocmd = function() end,
    },
    log = {
      error = function(text)
        error(text)
      end,
    },
  }
  local chunk = assert(loadstring(review_source))
  setfenv(
    chunk,
    setmetatable({
      maki = api,
      require = function(name)
        eq(name, "maki.text_input")
        return text_input
      end,
    }, { __index = _G })
  )
  chunk()
  return h
end

local function assert_closed(h)
  for _, win in ipairs(h.windows) do
    eq(win.closed, true, "review window left open")
  end
end

local function comment_events()
  return { "<CR>", "c", { type = "paste", text = COMMENT }, "<CR>" }
end

function submits_comments_to_a_focused_fix_session()
  local h = review()
  h.events = comment_events()
  table.insert(h.events, "s")
  h.open()
  eq(h.focused, FIX_SESSION)
  eq(#h.submissions, 1)
  eq(h.submissions[1].focus, true)
  local prompt = h.submissions[1].prompt
  has(prompt, "Address every comment")
  has(prompt, "## " .. FILE)
  has(prompt, "Comment 1 (line 1)")
  has(prompt, "> Fix this branch\n> Keep the existing behavior")
  has(prompt, "+let value_1 = 1;  <<< comment applies here")
  assert_closed(h)

  h.events = { "s", "q" }
  h.open()
  eq(#h.submissions, 1, "successful submission must clear comments")
  has(h.flashes[#h.flashes], "No review comments yet")
  assert_closed(h)
end

function failed_submission_keeps_review_responsive_and_comments(failure)
  local h = review()
  h.failure = failure
  h.events = comment_events()
  table.insert(h.events, "s")
  table.insert(h.events, function()
    eq(h.focused, nil)
    has(h.flashes[#h.flashes], SUBMISSION_ERROR)
    for _, win in ipairs(h.windows) do
      eq(win.closed, false)
    end
    has(line_text(h.buffers[3].lines[1]), FILE)
  end)
  table.insert(h.events, "j")
  table.insert(h.events, "q")
  h.open()
  assert_closed(h)

  h.failure = nil
  h.events = { "s" }
  h.open()
  eq(h.focused, FIX_SESSION)
  eq(#h.submissions, 2)
  eq(h.submissions[2].prompt, h.submissions[1].prompt, "failed submission lost comment data")
  assert_closed(h)
end

function navigation_moves_native_cursor_without_rebuilding_buffers()
  local h = review()
  local writes, highlights
  local function unchanged()
    for i, buf in ipairs(h.buffers) do
      eq(buf.writes, writes[i], "navigation rebuilt buffer " .. i)
    end
    eq(h.highlights, highlights)
    eq(h.latest[4].config.cursor_line, true)
  end
  h.events = {
    "<CR>",
    function()
      writes = {}
      for i, buf in ipairs(h.buffers) do
        writes[i] = buf.writes
      end
      highlights = h.highlights
      eq(highlights, 1)
      eq(h.latest[4].cursor, 2)
    end,
    "j",
    "k",
    "<PageDown>",
    "<PageUp>",
    "<Home>",
    function()
      unchanged()
      eq(h.latest[4].cursor, 1)
    end,
    "<End>",
    function()
      unchanged()
      eq(h.latest[4].cursor, DIFF_LINES + 1)
    end,
    "<Home>",
    "j",
    function()
      unchanged()
      eq(h.latest[4].cursor, 2)
      eq(h.buffers[4].lines[2][4][2].fg, SYNTAX_FG)
    end,
    "q",
  }
  h.open()
  assert_closed(h)
end

function range_editor_comment_rows_and_resize_preserve_navigation()
  local h = review()
  local range_bg, left_writes
  h.events = {
    "<CR>",
    "v",
    function()
      range_bg = h.buffers[4].lines[2][4][2].bg
      left_writes = { h.buffers[1].writes, h.buffers[2].writes, h.buffers[3].writes }
    end,
    "j",
    function()
      for i, writes in ipairs(left_writes) do
        eq(h.buffers[i].writes, writes)
      end
      eq(h.buffers[4].lines[3][4][2].bg, range_bg)
      eq(h.buffers[4].lines[3][4][2].fg, SYNTAX_FG)
      assert(h.buffers[4].lines[4][4][2].bg ~= range_bg)
    end,
    "c",
    function()
      eq(h.latest[4].config.cursor_line, false)
    end,
    { type = "paste", text = COMMENT .. string.rep(" long text", 20) },
    "<CR>",
    function()
      eq(h.latest[4].config.cursor_line, true)
    end,
    "j",
    function()
      local row = h.latest[4].cursor
      assert(row > 4, "navigation did not skip inline comment rows")
      has(line_text(h.buffers[4].lines[row]), "let value_3 = 3;")
    end,
    "k",
    "c",
    { type = "paste", text = " edited" },
    "<CR>",
    function()
      h.cols = INPUT_WIDTH + 20
    end,
    { type = "resize" },
    function()
      eq(#h.windows, 8)
      for i = 1, 4 do
        eq(h.windows[i].closed, true)
      end
      eq(h.latest[4].config.cursor_line, true)
    end,
    "s",
  }
  h.open()
  eq(h.highlights, 1, "resize re-highlighted unchanged source")
  has(h.submissions[1].prompt, "Comment 1 (lines 1-2)")
  has(h.submissions[1].prompt, " edited")
  assert_closed(h)
end

function typing_deep_in_a_large_diff_only_renders_editor_context()
  local h = review()
  local left_writes
  local function cursor_on_input()
    local row = h.buffers[4].lines[h.latest[4].cursor]
    for _, span in ipairs(row) do
      if span[2] == "cursor" then
        return
      end
    end
    error("native cursor does not follow the text input cursor row")
  end
  local function bounded()
    assert(#h.buffers[4].lines <= h.latest[4].height + 20, "typing rebuilt the whole diff")
    for i, writes in ipairs(left_writes) do
      eq(h.buffers[i].writes, writes, "typing rebuilt left panel " .. i)
    end
    eq(h.highlights, 1)
    cursor_on_input()
  end
  local function remember_left()
    left_writes = { h.buffers[1].writes, h.buffers[2].writes, h.buffers[3].writes }
    bounded()
  end
  local function full_diff(line)
    assert(#h.buffers[4].lines >= DIFF_LINES + 1, "closing editor did not restore full diff")
    has(line_text(h.buffers[4].lines[h.latest[4].cursor]), "let value_" .. line .. " =")
    eq(h.latest[4].config.cursor_line, true)
  end
  h.events = { "<CR>", "<End>", "c", remember_left }
  for char in TYPED_COMMENT:gmatch(".") do
    table.insert(h.events, char == " " and "<Space>" or char)
    table.insert(h.events, bounded)
  end
  local tail = {
    { type = "paste", text = "\n" .. string.rep("wrapped text ", 20) },
    "<Up>",
    "<Home>",
    bounded,
    function()
      h.cols = INPUT_WIDTH + 20
    end,
    { type = "resize" },
    remember_left,
    "<Esc>",
    function()
      full_diff(DIFF_LINES)
    end,
    "k",
    function()
      full_diff(DIFF_LINES - 1)
    end,
    "c",
    remember_left,
  }
  for _, event in ipairs(tail) do
    table.insert(h.events, event)
  end
  for char in TYPED_COMMENT:gmatch(".") do
    table.insert(h.events, char == " " and "<Space>" or char)
    table.insert(h.events, bounded)
  end
  table.insert(h.events, "<CR>")
  table.insert(h.events, function()
    full_diff(DIFF_LINES - 1)
  end)
  table.insert(h.events, "s")
  h.open()
  has(h.submissions[1].prompt, "Comment 1 (line " .. DIFF_LINES - 1 .. ")")
  has(h.submissions[1].prompt, "> " .. TYPED_COMMENT)
  assert_closed(h)
end

function render_failure_closes_modal_windows_and_preserves_comments()
  local h = review()
  h.events = comment_events()
  table.insert(h.events, function()
    h.render_failure = true
  end)
  table.insert(h.events, "c")
  local ok, err = pcall(h.open)
  eq(ok, false)
  has(tostring(err), RENDER_ERROR)
  assert_closed(h)

  h.render_failure = nil
  h.events = { "s" }
  h.open()
  eq(h.focused, FIX_SESSION)
  has(h.submissions[1].prompt, "> Fix this branch\n> Keep the existing behavior")
  assert_closed(h)
end
