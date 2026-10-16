-- Tab completion of file paths in the chat input, in Lua.
--
-- Tab is the host's mode-toggle key. A plugin binding sees it before the
-- toggle, so this owns Tab: on a path token it lists the directory and opens a
-- popup; on anything else it hands the key back to the toggle through the same
-- builtin the default binding runs, so the documented Tab behaviour is never
-- re-implemented here and cannot drift from it.
--
-- While the popup is up it claims `<Tab>` itself, so accepting a row never
-- reaches this handler and the toggle is only reached with no popup.

local Events = require("tc.events")
local Menu = require("tc.menu")
local Path = require("tc.path")

maki.keymap.set("n", "<Tab>", function()
  if Menu.session_id() then
    return
  end
  local st = maki.ui.input()
  if not st then
    return
  end
  local _, token = Path.parse(st.text, st.cursor)
  if not Path.is_path(token) then
    maki.ui.action("mode_toggle")
    return
  end
  Menu.refresh_later(st)
end, { desc = "Complete a file path, else toggle mode" })

maki.api.create_autocmd("InputChanged", {
  callback = function(ev)
    Events.input_changed(ev.data)
  end,
})

maki.api.create_autocmd("SessionReset", {
  callback = function(ev)
    Events.session_reset(ev.data)
  end,
})

maki.api.create_autocmd("TaskFocusChanged", {
  callback = Menu.close,
})

maki.api.create_autocmd("SessionStatusChanged", {
  callback = function(ev)
    Events.session_status_changed(ev.data)
  end,
})
