local CHANGE = "src/main.rs\n"
local UNTRACKED = "notes.txt\n"
local NUDGE = "2 file(s) changed — /review to inspect & comment"

local function harness()
  local h = { tracked = CHANGE, untracked = UNTRACKED, flashes = {}, commands = 0 }
  local api = {
    fn = {
      jobstart = function(args)
        return args[2]
      end,
      jobwait = function(command)
        if command == "diff" then
          return { exit_code = h.unborn and 128 or 0, stdout = h.tracked }
        end
        return { exit_code = h.failed and 1 or 0, stdout = h.untracked }
      end,
    },
    ui = {
      flash = function(message)
        h.flashes[#h.flashes + 1] = message
      end,
    },
    async = { run = function(callback) callback() end },
    api = {
      register_command = function() h.commands = h.commands + 1 end,
      create_autocmd = function(event, opts)
        assert(event == "TurnEnd")
        h.turn_end = opts.callback
      end,
    },
  }
  local chunk = assert(loadstring(review_source))
  setfenv(chunk, setmetatable({ maki = api }, { __index = _G }))
  chunk()
  return h
end

return {
  changed_files_nudge_without_registering_a_command = function()
    local h = harness()
    h.turn_end()
    assert(h.commands == 0)
    assert(h.flashes[1] == NUDGE)
  end,
  unchanged_signature_does_not_repeat = function()
    local h = harness()
    h.turn_end()
    h.turn_end()
    assert(#h.flashes == 1)
  end,
  clean_tree_resets_the_nudge = function()
    local h = harness()
    h.turn_end()
    h.tracked, h.untracked = "", ""
    h.turn_end()
    h.tracked, h.untracked = CHANGE, UNTRACKED
    h.turn_end()
    assert(#h.flashes == 2)
    assert(h.flashes[2] == NUDGE)
  end,
  unborn_head_still_reports_untracked_files = function()
    local h = harness()
    h.unborn = true
    h.turn_end()
    assert(h.flashes[1] == "1 file(s) changed — /review to inspect & comment")
  end,
  failed_git_query_does_not_nudge = function()
    local h = harness()
    h.failed = true
    h.turn_end()
    assert(#h.flashes == 0)
  end,
}
