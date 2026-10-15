local ToolView = require("maki.tool_view")

local ACTIONS = {
  screenshot = true,
  mouse = true,
  keyboard = true,
  position = true,
  display = true,
  clipboard = true,
}

local function summarize(input)
  if input.action == "mouse" then
    return "mouse " .. (input.mouse_action or "")
  elseif input.action == "keyboard" then
    if input.text then
      return "type " .. input.text
    end
    return "key " .. (input.key or "")
  elseif input.action == "clipboard" then
    if input.text then
      return "clipboard set text"
    end
    if input.image_path then
      return "clipboard set image " .. input.image_path
    end
    return "clipboard get" .. (input.path and " image → " .. input.path or "")
  else
    return input.action
  end
end

local function create_computer_view(input)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, { max_lines = 5, keep = "tail" })
  view:set_header({ { summarize(input) } })
  buf:on("click", function()
    view:toggle()
  end)
  return buf, view
end

local description = [[Control the desktop: mouse, keyboard, screenshots, clipboard.

Actions:
- screenshot: capture the whole screen to a PNG; view it with view_image.
- mouse: action=move|click|press|release|scroll with x,y, button (left/right/middle), amount, axis.
- keyboard: text to type, or key as a name/combo (enter, escape, tab, up, ctrl+c, f1).
- position: report the mouse pointer position.
- display: report the screen size.
- clipboard: get text (default), get image with path, set text, or set image from image_path.

Needs a display server (X11/XWayland); calls fail cleanly without one. Screenshot before acting blind, and after acting to verify.]]

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- computer: screenshot first to see the screen, act, screenshot again to verify. Keyboard keys are names like enter/ctrl+c, not raw codes.",
})

maki.api.register_tool({
  name = "computer",
  kind = "execute",
  description = description,
  schema = {
    type = "object",
    properties = {
      action = {
        type = "string",
        description = "One of: screenshot, mouse, keyboard, position, display, clipboard",
        required = true,
      },
      path = { type = "string", description = "File to write the screenshot to (default: maki state dir)" },
      mouse_action = { type = "string", description = "mouse: move | click | press | release | scroll" },
      x = { type = "integer", description = "mouse move: absolute x; clipboard: unused" },
      y = { type = "integer", description = "mouse move: absolute y" },
      button = { type = "string", description = "mouse: left | right | middle (default left)" },
      amount = { type = "integer", description = "mouse scroll: clicks (default 3)" },
      axis = { type = "string", description = "mouse scroll: vertical | horizontal (default vertical)" },
      text = { type = "string", description = "keyboard: literal text to type; clipboard: text to set" },
      key = { type = "string", description = "keyboard: key name or combo, e.g. enter, ctrl+shift+s" },
      image_path = { type = "string", description = "clipboard: PNG file to copy onto the clipboard" },
      get = { type = "boolean", description = "clipboard: request text (default true)" },
    },
  },
  permission = "run",
  permission_scopes = function(input)
    if not input.action then
      return nil
    end
    return { scopes = { input.action }, force_prompt = false }
  end,

  header = function(input)
    return summarize(input)
  end,

  restore = function(input, output, is_error)
    local buf, view = create_computer_view(input)
    if is_error then
      view:append({ { output, "dim" } })
    else
      view:append_text(output)
    end
    view:finish()
    return buf
  end,

  handler = function(input)
    local action = input.action
    if not action or not ACTIONS[action] then
      return {
        llm_output = "error: action must be one of screenshot, mouse, keyboard, position, display, clipboard",
        is_error = true,
      }
    end

    if action == "screenshot" then
      local r, err = maki.computer.screenshot({ path = input.path })
      if not r then
        return { llm_output = "error: " .. err, is_error = true }
      end
      return { llm_output = "saved " .. r.path .. " (" .. r.width .. "x" .. r.height .. ")" }
    elseif action == "mouse" then
      local r, err = maki.computer.mouse({
        action = input.mouse_action,
        x = input.x,
        y = input.y,
        button = input.button,
        amount = input.amount,
        axis = input.axis,
      })
      if not r then
        return { llm_output = "error: " .. err, is_error = true }
      end
      return { llm_output = "ok" }
    elseif action == "keyboard" then
      local r, err = maki.computer.keyboard({ text = input.text, key = input.key })
      if not r then
        return { llm_output = "error: " .. err, is_error = true }
      end
      return { llm_output = "ok" }
    elseif action == "position" then
      local r, err = maki.computer.position({})
      if not r then
        return { llm_output = "error: " .. err, is_error = true }
      end
      return { llm_output = r.x .. "," .. r.y }
    elseif action == "display" then
      local r, err = maki.computer.display({})
      if not r then
        return { llm_output = "error: " .. err, is_error = true }
      end
      return { llm_output = r.width .. "x" .. r.height }
    else
      local r, err = maki.computer.clipboard({
        text = input.text,
        image_path = input.image_path,
        path = input.path,
        get = input.get,
      })
      if not r then
        return { llm_output = "error: " .. err, is_error = true }
      end
      if r.set then
        return { llm_output = "clipboard set" }
      end
      if r.path then
        return { llm_output = "saved " .. r.path .. " (" .. r.width .. "x" .. r.height .. ")" }
      end
      return { llm_output = r.text or "" }
    end
  end,
})
