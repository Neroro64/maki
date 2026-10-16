local Path = require("tc.path")
local Menu = require("tc.menu")
local Events = require("tc.events")
local Skills = require("maki.skills")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

-- ---------------------------------------------------------------- path.parse

case("parse_takes_the_token_to_the_cursor", function()
  local start, token = Path.parse("F:/SomeF", 8)
  eq(start, 0, "the token starts at the first byte")
  eq(token, "F:/SomeF", "a slash is not a delimiter, so the whole path is one token")
end)

case("parse_stops_at_a_space", function()
  local start, token = Path.parse("read F:/SomeF", 13)
  eq(start, 5, "the token begins after the space")
  eq(token, "F:/SomeF")
end)

case("parse_stops_at_a_quote_and_an_equals", function()
  eq(Path.parse('open "src/ma', 12), 6)
  eq(select(2, Path.parse('open "src/ma', 12)), "src/ma")
  eq(select(2, Path.parse("out=src/ma", 10)), "src/ma")
end)

case("parse_ignores_text_past_the_cursor", function()
  local start, token = Path.parse("./src and more", 5)
  eq(start, 0)
  eq(token, "./src", "the caret is inside the token, not past it")
end)

case("parse_counts_bytes_not_characters", function()
  local start, token = Path.parse("héllo wörld ./x", 17)
  eq(start, 14, "two two-byte letters push the token two bytes along")
  eq(token, "./x")
end)

case("parse_of_a_bare_word_is_that_word", function()
  local start, token = Path.parse("main", 4)
  eq(start, 0)
  eq(token, "main")
end)

-- ---------------------------------------------------------------- path.is_path

case("is_path_wants_a_separator_a_home_a_dot_a_drive", function()
  for _, token in ipairs({ "F:/x", "./x", "../x", "~/x", "/x", "src/main", "F:", "." }) do
    eq(Path.is_path(token), true, token .. " is a path")
  end
  for _, token in ipairs({ "main", "hello world", "" }) do
    eq(Path.is_path(token), false, token .. " is not a path, so Tab stays the mode toggle")
  end
end)

-- ------------------------------------------------------------- path.skill_prefix

case("skill_prefix_takes_the_name_after_the_colon", function()
  eq(Path.skill_prefix("/skill:git"), "git")
  eq(Path.skill_prefix("/skill:"), "", "an empty prefix lists every skill")
  eq(Path.skill_prefix("/skill"), nil, "no colon is not a skill mention")
  eq(Path.skill_prefix("F:/x"), nil, "a path is not a skill mention")
end)

-- -------------------------------------------------------------- path.resolve

local CWD = "/repo"
local HOME = "/home/u"

case("resolve_a_drive_prefix_lists_that_drive", function()
  local r = Path.resolve("F:/SomeF", CWD, HOME)
  eq(r.dir, "F:/", "the directory to list")
  eq(r.prefix, "SomeF", "the name being typed")
  eq(r.disp, "F:/", "the completion keeps the drive shape")
end)

case("resolve_a_trailing_slash_lists_the_directory_whole", function()
  local r = Path.resolve("F:/", CWD, HOME)
  eq(r.dir, "F:/")
  eq(r.prefix, "")
  eq(r.disp, "F:/")
end)

case("resolve_expands_home_and_keeps_the_tilde_shape", function()
  local r = Path.resolve("~/foo", CWD, HOME)
  eq(r.dir, HOME .. "/", "the listing is under home")
  eq(r.prefix, "foo")
  eq(r.disp, "~/", "the completion writes `~/foo`, not the expanded home")
end)

case("resolve_a_bare_tilde_lists_home", function()
  local r = Path.resolve("~", CWD, HOME)
  eq(r.dir, HOME .. "/")
  eq(r.prefix, "")
  eq(r.disp, "~/")
end)

case("resolve_a_relative_parent_lists_the_parent", function()
  local r = Path.resolve("./../SomeF", CWD, HOME)
  eq(r.dir, "/repo/./../", "relative to the session tree")
  eq(r.prefix, "SomeF")
  eq(r.disp, "./../")
end)

case("resolve_a_bare_word_lists_the_working_tree", function()
  local r = Path.resolve("main", CWD, HOME)
  eq(r.dir, "/repo/")
  eq(r.prefix, "main")
  eq(r.disp, "", "the completion is just the name")
end)

-- ------------------------------------------------------------- path.entries

case("entries_filter_by_prefix_and_skip_dot_git", function()
  local raw = { { "src", "directory" }, { "main.rs", "file" }, { ".git", "directory" }, { "sub", "directory" } }
  local rows = Path.entries(raw, "src/", "m", 10)
  eq(#rows, 1, "only main.rs starts with m, and .git is never offered")
  eq(rows[1].label, "main.rs")
  eq(rows[1].insert, "src/main.rs", "the display prefix is prepended")
  eq(rows[1].is_dir, false)
end)

case("entries_put_directories_first_with_a_trailing_slash", function()
  local raw = { { "b.txt", "file" }, { "a", "directory" }, { "c.md", "file" }, { "d", "directory" } }
  local rows = Path.entries(raw, "", "", 10)
  eq(rows[1].label, "a/")
  eq(rows[2].label, "d/")
  eq(rows[3].label, "b.txt")
  eq(rows[4].label, "c.md")
end)

case("entries_highlight_the_matched_prefix", function()
  local rows = Path.entries({ { "main.rs", "file" } }, "", "ma", 10)
  eq(rows[1].highlights[1][1], 1)
  eq(rows[1].highlights[1][2], 2, "the two matched bytes")
  eq(#Path.entries({ { "x", "file" } }, "", "", 10)[1].highlights, 0, "no prefix, no highlight")
end)

case("entries_cap_at_the_limit", function()
  local raw = {}
  for i = 1, 20 do
    raw[i] = { "f" .. i, "file" }
  end
  eq(#Path.entries(raw, "", "", 5), 5)
end)

-- ------------------------------------------------------------------ menu

local SESSION = "session-1"
local OTHER_SESSION = "session-2"
local INPUT = { text = "read F:/Some", cursor = 12, version = 3, session_id = SESSION }

-- Stubs every host call the menu makes. `h.dir` is what `maki.fs.dir` answers;
-- `h.edits`, `h.rows`, `h.claimed`, `h.opens`, `h.closes`, `h.flashes` are what
-- the menu did. The fake window answers `recv` with a close, so the key loop
-- ends where it starts and a case drives keys through `Menu.handle_key`.
local function harness(body)
  local saved = {
    input = maki.ui.input,
    input_edit = maki.ui.input_edit,
    buf = maki.ui.buf,
    open_win = maki.ui.open_win,
    terminal_size = maki.ui.terminal_size,
    display_width = maki.ui.display_width,
    flash = maki.ui.flash,
    dir = maki.fs.dir,
    normalize = maki.fs.normalize,
    abspath = maki.fs.abspath,
    session = maki.session.read,
    run = maki.async.run,
    warn = maki.log.warn,
  }
  local h = {
    dir = { { "SomeFile", "file" }, { "SomeDir", "directory" } },
    edits = {},
    rows = {},
    flashes = {},
    claimed = nil,
    focus = nil,
    opens = 0,
    closes = 0,
  }

  maki.ui.input = function()
    return h.input
  end
  maki.ui.input_edit = function(edit)
    table.insert(h.edits, edit)
    if h.refuse_edit then
      return nil, h.refuse_edit
    end
    return true
  end
  maki.ui.buf = function()
    return { set_lines = function(_, painted) h.rows = painted end }
  end
  maki.ui.open_win = function(_, win_opts)
    h.opens = h.opens + 1
    h.claimed = win_opts.keys
    h.focus = win_opts.focus
    return {
      set_config = function() end,
      show = function() end,
      recv = function() return { type = "close" } end,
      close = function(self)
        assert(self ~= nil, "win:close() must be called as a method with self")
        h.closes = h.closes + 1
      end,
    }
  end
  maki.ui.terminal_size = function() return { cols = 80 } end
  maki.ui.display_width = function(s) return #s end
  maki.ui.flash = function(msg) table.insert(h.flashes, msg) end
  maki.fs.dir = function() return h.dir end
  maki.fs.normalize = function(p) return p end
  maki.fs.abspath = function(p) return p == "~" and HOME or p end
  maki.session.read = function() return { cwd = CWD } end
  maki.async.run = function(fn) fn() end
  maki.log.warn = function() end

  local ok, err = pcall(body, h)

  Menu.close()
  maki.ui.input, maki.ui.input_edit = saved.input, saved.input_edit
  maki.ui.buf, maki.ui.open_win = saved.buf, saved.open_win
  maki.ui.terminal_size, maki.ui.display_width, maki.ui.flash = saved.terminal_size, saved.display_width, saved.flash
  maki.fs.dir, maki.fs.normalize, maki.fs.abspath = saved.dir, saved.normalize, saved.abspath
  maki.session.read, maki.async.run = saved.session, saved.run
  maki.log.warn = saved.warn
  if not ok then
    error(err)
  end
  return h
end

local function row(h, i)
  if not h.rows[i] then
    return nil
  end
  local text = {}
  for _, span in ipairs(h.rows[i]) do
    text[#text + 1] = span[1]
  end
  return table.concat(text)
end

case("refresh_lists_the_directory_the_token_names", function()
  local h = harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
  end)
  eq(row(h, 1), " SomeDir/", "directories first")
  eq(row(h, 2), " SomeFile")
  eq(h.focus, false, "the chat input keeps the keyboard")
end)

case("the_popup_claims_tab_to_accept", function()
  local h = harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
  end)
  eq(table.concat(h.claimed, " "):find("<Tab>", 1, true) ~= nil, true, "Tab accepts a row")
  eq(h.closes, 1, "and the close hands the keys back")
end)

case("accept_writes_the_row_over_the_token", function()
  local h = harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
    Menu.accept()
  end)
  eq(#h.edits, 1)
  eq(h.edits[1].text, "F:/SomeDir/", "the display prefix plus the directory row")
  eq(h.edits[1].start, 5, "the token starts after the space")
  eq(h.edits[1].stop, INPUT.cursor, "and runs to the caret")
  eq(h.edits[1].version, INPUT.version)
  eq(h.edits[1].session_id, SESSION)
end)

case("tab_and_enter_both_accept", function()
  for _, key in ipairs({ "<Tab>", "<CR>" }) do
    local h = harness(function(hh)
      hh.input = INPUT
      Menu.refresh(hh.input)
      Menu.handle_key(key)
    end)
    eq(#h.edits, 1, key .. " inserted the row")
  end
end)

case("the_navigation_keys_move_the_highlight", function()
  local h = harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
    Menu.handle_key("<Down>")
    Menu.handle_key("<Tab>")
  end)
  eq(h.edits[1].text, "F:/SomeFile", "down landed on the file row")
end)

case("esc_closes_the_popup", function()
  local h = harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
    Menu.handle_key("<Esc>")
    eq(Menu.session_id(), nil)
  end)
  eq(h.closes, 1)
  eq(#h.edits, 0, "and nothing was inserted")
end)

case("a_token_that_stops_being_a_path_closes_the_popup", function()
  local h = harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
    eq(Menu.session_id(), SESSION)
    Menu.refresh({ text = "read word", cursor = 9, version = 4, session_id = SESSION })
    eq(Menu.session_id(), nil, "the caret left the path, so the rows are gone")
  end)
  eq(h.opens, 1, "no window reopened for the non-path")
end)

case("an_empty_listing_shows_no_matches", function()
  local h = harness(function(hh)
    hh.input = INPUT
    hh.dir = {}
    Menu.refresh(hh.input)
  end)
  eq(row(h, 1), " no matches")
end)

case("accept_reports_a_refused_edit", function()
  local REFUSED = "input changed since version 3"
  local h = harness(function(hh)
    hh.input = INPUT
    hh.refuse_edit = REFUSED
    Menu.refresh(hh.input)
    Menu.accept()
  end)
  eq(#h.flashes, 1)
  th.has(h.flashes[1], REFUSED, "the host's reason reaches the user")
end)

-- ----------------------------------------------------------- skill completion

local SKILLS = {
  { name = "git-release", description = "Cut a release", location = "l", enabled = true },
  { name = "privacy", description = "Privacy", location = "l", enabled = true },
  { name = "off", description = "Off", location = "l", enabled = false },
}

case("a_skill_token_lists_enabled_skills_by_prefix", function()
  Skills.set(SKILLS)
  local h = harness(function(hh)
    hh.input = { text = "/skill:", cursor = 7, version = 3, session_id = SESSION }
    Menu.refresh(hh.input)
  end)
  eq(row(h, 1), " git-release", "enabled skills, name order")
  eq(row(h, 2), " privacy")
  eq(row(h, 3), nil, "a disabled skill is not offered")
  Skills.set({})
end)

case("a_skill_prefix_narrows_the_rows", function()
  Skills.set(SKILLS)
  local h = harness(function(hh)
    hh.input = { text = "/skill:pr", cursor = 9, version = 3, session_id = SESSION }
    Menu.refresh(hh.input)
  end)
  eq(row(h, 1), " privacy", "only the prefix match is left")
  Skills.set({})
end)

case("accept_writes_the_whole_skill_token", function()
  Skills.set(SKILLS)
  local h = harness(function(hh)
    hh.input = { text = "/skill:gi", cursor = 8, version = 3, session_id = SESSION }
    Menu.refresh(hh.input)
    Menu.accept()
  end)
  eq(h.edits[1].text, "/skill:git-release", "the insert carries the /skill: shape")
  eq(h.edits[1].start, 0, "the token starts at the slash")
  eq(h.edits[1].stop, 8, "and runs to the caret")
  Skills.set({})
end)

-- ---------------------------------------------------------------- events

case("events_ignore_this_plugins_own_edit", function()
  harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
    Events.input_changed({
      text = INPUT.text,
      cursor = INPUT.cursor,
      version = INPUT.version,
      session_id = SESSION,
      source = Events.PLUGIN,
    })
    eq(Menu.session_id(), SESSION, "an accept must not reopen the popup on what it inserted")
  end)
end)

case("events_narrow_the_popup_as_the_user_types", function()
  harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
    Events.input_changed({
      text = "read F:/SomeFile",
      cursor = 16,
      version = 4,
      session_id = SESSION,
    })
    eq(Menu.session_id(), SESSION, "typing inside the path keeps it up")
  end)
end)

case("events_never_open_the_popup_from_typing_alone", function()
  harness(function(hh)
    Events.input_changed({ text = "read F:/", cursor = 8, version = 1, session_id = SESSION })
    eq(Menu.session_id(), nil, "only Tab opens it")
  end)
end)

case("events_close_a_popup_when_another_tab_takes_focus", function()
  harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
    Events.input_changed({ text = "x", cursor = 1, version = 1, session_id = OTHER_SESSION })
    eq(Menu.session_id(), nil)
  end)
end)

case("events_close_a_popup_when_its_session_resets", function()
  harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
    Events.session_reset({ session_id = OTHER_SESSION })
    eq(Menu.session_id(), SESSION, "a reset in another tab leaves this one alone")
    Events.session_reset({ session_id = SESSION })
    eq(Menu.session_id(), nil)
  end)
end)

case("events_close_a_popup_when_the_input_goes_off_screen", function()
  harness(function(hh)
    hh.input = INPUT
    Menu.refresh(hh.input)
    Events.session_status_changed({ focused = false, status = "needs_input" })
    eq(Menu.session_id(), SESSION)
    Events.session_status_changed({ focused = true, status = "needs_input" })
    eq(Menu.session_id(), nil)
  end)
end)

th.report()
