local ToolView = require("maki.tool_view")

local SEPARATOR = "──────"

local ACTIONS = {
  open = true,
  snapshot = true,
  click = true,
  type = true,
  press = true,
  scroll = true,
  screenshot = true,
  eval = true,
  status = true,
  close = true,
}

local function summarize(input)
  if input.action == "open" then
    return "open " .. (input.url or "")
  elseif input.action == "click" then
    return "click #" .. tostring(input.index)
  elseif input.action == "type" then
    return "type " .. (input.index and "#" .. tostring(input.index) .. " " or "") .. (input.text or "")
  elseif input.action == "press" then
    return "press " .. (input.key or "")
  elseif input.action == "scroll" then
    return "scroll " .. tostring(input.x or 0) .. "," .. tostring(input.y or 0)
  elseif input.action == "screenshot" then
    return "screenshot " .. (input.path or "")
  elseif input.action == "eval" then
    return "eval " .. (input.source or "")
  else
    return input.action
  end
end

local function create_browser_view(input)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, { max_lines = 10, keep = "tail" })
  view:set_header({ { summarize(input) } })
  buf:on("click", function()
    view:toggle()
  end)
  return buf, view
end

local description = [[Drive a Chrome browser over the DevTools protocol.

Actions:
- open: launch the browser (headless by default) and navigate to a URL.
- snapshot: list interactive elements with indices; always snapshot before click/type.
- click: click the element with the given index.
- type: type text into an indexed element (focused first) or the focused one.
- press: press a key like Enter, Escape, Tab.
- scroll: scroll the page by x,y pixels.
- screenshot: save a PNG of the page to a path; view it with view_image.
- eval: evaluate a JavaScript expression and return its JSON result.
- status: report the current URL and title.
- close: kill the browser process.

The browser stays alive across calls for the whole session. Indices from a snapshot go stale after navigation — snapshot again.]]

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- browser: snapshot first, then click/type by index; re-snapshot after navigation. Screenshots need view_image to inspect.",
})

maki.api.register_tool({
  name = "browser",
  kind = "execute",
  description = description,
  schema = {
    type = "object",
    properties = {
      action = {
        type = "string",
        description = "One of: open, snapshot, click, type, press, scroll, screenshot, eval, status, close",
        required = true,
      },
      url = { type = "string", description = "URL to open (action=open)" },
      headless = { type = "boolean", description = "Launch without a window (default true, action=open)" },
      executable = {
        type = "string",
        description = "Chrome/Chromium binary path (action=open, default auto-discovered)",
      },
      index = { type = "integer", description = "Element index from the latest snapshot (action=click/type)" },
      text = { type = "string", description = "Text to type (action=type)" },
      key = { type = "string", description = "Key to press, e.g. Enter (action=press)" },
      x = { type = "integer", description = "Horizontal scroll pixels (action=scroll)" },
      y = { type = "integer", description = "Vertical scroll pixels (action=scroll)" },
      source = { type = "string", description = "JavaScript expression (action=eval)" },
      path = { type = "string", description = "File to write the PNG to (action=screenshot)" },
    },
  },
  permission = "net",
  permission_scopes = function(input)
    if input.action == "open" and input.url then
      return { scopes = { input.url }, force_prompt = false }
    end
    if input.action == "eval" and input.source then
      return { scopes = { input.source }, force_prompt = false }
    end
    return nil
  end,

  header = function(input)
    return summarize(input)
  end,

  restore = function(input, output, is_error)
    local buf, view = create_browser_view(input)
    if is_error then
      view:append({ { output, "dim" } })
    else
      view:append_text(output)
    end
    view:finish()
    return buf
  end,

  handler = function(input, ctx)
    local action = input.action
    if not action or not ACTIONS[action] then
      return {
        llm_output = "error: action must be one of open, snapshot, click, type, press, scroll, screenshot, eval, status, close",
        is_error = true,
      }
    end
    local session = ctx:session_id()
    if not session then
      return { llm_output = "error: browser needs a session", is_error = true }
    end

    local opts = {
      session = session,
      url = input.url,
      headless = input.headless,
      executable = input.executable,
      index = input.index,
      text = input.text,
      key = input.key,
      x = input.x,
      y = input.y,
      source = input.source,
      path = input.path,
    }
    local r, err = maki.browser[action](opts)
    if not r and err then
      return { llm_output = "error: " .. err, is_error = true }
    end

    if action == "open" then
      return { llm_output = "opened " .. (r.url or "") .. " — " .. (r.title or "") }
    elseif action == "snapshot" then
      local lines = { (r.url or "") .. " — " .. (r.title or "") .. " (" .. (r.count or 0) .. " elements)" }
      for _, el in ipairs(r.elements or {}) do
        local label = el.text ~= "" and el.text or (el.href ~= "" and el.href or "")
        lines[#lines + 1] = string.format(
          "[%d] %s%s%s%s",
          el.idx,
          el.tag,
          el.type ~= "" and "/" .. el.type or "",
          el.role ~= "" and "/" .. el.role or "",
          label ~= "" and ' "' .. label .. '"' or ""
        )
      end
      return { llm_output = table.concat(lines, "\n") }
    elseif action == "screenshot" then
      return { llm_output = "saved " .. r }
    elseif action == "eval" then
      return { llm_output = maki.json.encode(r) }
    elseif action == "status" then
      if not r.open then
        return { llm_output = "no browser open" }
      end
      return { llm_output = (r.url or "") .. " — " .. (r.title or "") }
    elseif action == "close" then
      return { llm_output = "browser closed" }
    else
      return { llm_output = "ok" }
    end
  end,
})
