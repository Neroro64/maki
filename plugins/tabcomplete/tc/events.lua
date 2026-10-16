-- What the host's events mean for the popup: which take it away, and which are
-- this plugin's own writes coming back around. Separate from `init.lua` so the
-- decisions are testable without a running event loop.

local Menu = require("tc.menu")

local Events = {}

-- The name the host stamps on this plugin's own `input_edit`, so the
-- `InputChanged` handler tells an accept from the user typing.
Events.PLUGIN = "tabcomplete"

-- The popup narrows as the user keeps typing. A caret that has left the token
-- is handled inside `refresh`, which closes when the token stops being a path.
-- Typing alone never opens the popup: only Tab does.
function Events.input_changed(data)
  if data.source == Events.PLUGIN then
    return
  end
  local open = Menu.session_id()
  if open and open ~= data.session_id then
    Menu.close()
    return
  end
  if not Menu.session_id() then
    return
  end
  Menu.refresh_later(data)
end

function Events.session_reset(data)
  if Menu.session_id() == data.session_id then
    Menu.close()
  end
end

-- Cycling to a subagent tab leaves the popup holding `<Tab>` over a tab the
-- user is not looking at, where it would accept into the wrong input.
function Events.task_focus_changed()
  Menu.close()
end

-- A permission prompt, the plan form or a pack review takes the chat input off
-- screen, and the host then refuses to edit it.
function Events.session_status_changed(data)
  if data.focused and data.status == "needs_input" then
    Menu.close()
  end
end

return Events
