local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")

local SEPARATOR = "──────"
local DEADLINE_GRACE_SECS = 5

local function build_header_lines(command)
  local header = {}
  local highlighted = maki.ui.highlight(command, "powershell")
  if highlighted then
    for _, line in ipairs(highlighted) do
      header[#header + 1] = line
    end
  else
    header[#header + 1] = command
  end
  header[#header + 1] = { { SEPARATOR, "dim" } }
  return header
end

local function create_pwsh_view(command, ctx)
  local tol = ctx:tool_output_lines()
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, {
    max_lines = (tol and tol.pwsh) or 5,
    keep = "tail",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
  })
  view:set_header(build_header_lines(command))
  buf:on("click", function()
    view:toggle()
  end)
  return buf, view
end

local description = [[Execute a PowerShell command in a persistent session.

The PowerShell process stays alive for the whole agent session: variables, functions, and the working directory carry over between calls. Use it for PowerShell-specific work (cmdlets, $variables, object pipelines) and on Windows; use bash for everything else.

- Read, list, search, and edit files with dedicated tools, not PowerShell cmdlets.
- State persists: set a variable in one call, read it in the next.
- A timed-out call kills the process; the next call starts fresh with no state.
- Output sent to the model is truncated beyond 2000 lines or 50KB.
- Provide a short `description` (3-5 words).
- Use the `tail` param, not `| tail`: piping hides live output.]]

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use pwsh only for PowerShell-specific work; bash covers the rest. pwsh keeps one process alive per session, so variables and cwd persist across calls.",
})

local opts = maki.api.register_options(output_limits.extend({
  timeout_secs = {
    default = 120,
    min = 5,
    desc = "Kill the persistent session after this many seconds. A call's `timeout` param overrides it.",
  },
}))

maki.api.register_tool({
  name = "pwsh",
  kind = "execute",
  description = description,
  schema = {
    type = "object",
    properties = {
      command = { type = "string", description = "The PowerShell command to execute", required = true },
      timeout = { type = "integer", description = "Timeout in seconds (default 120)" },
      tail = { type = "integer", description = "Return only the last N lines" },
      reset = { type = "boolean", description = "Kill the persistent session first, discarding all state" },
      description = { type = "string", description = "Short description (3-5 words) of what the command does" },
    },
  },
  permission = "run",
  permission_scopes = function(input)
    local command = input.command
    if not command or command:match("^%s*$") then
      return nil
    end
    return { scopes = { command }, force_prompt = false }
  end,

  header = function(input)
    local s = input.description or input.command
    local hints = {}
    if input.timeout then
      hints[#hints + 1] = maki.ui.humantime(input.timeout) .. " timeout"
    end
    if input.tail then
      hints[#hints + 1] = "tail " .. input.tail
    end
    if input.reset then
      hints[#hints + 1] = "reset"
    end
    if #hints == 0 then
      return s
    end
    local buf = maki.ui.buf()
    buf:line({ { s }, { " (" .. table.concat(hints, ", ") .. ")", "dim" } })
    return buf
  end,

  restore = function(input, output, is_error, ctx)
    local buf, view = create_pwsh_view(input.command, ctx)
    if is_error then
      local body, code = output:match("^(.-)\nExit code: (%d+)$")
      if body then
        view:append_text(body)
        view:append({ { "Exit code: " .. code, "dim" } })
      else
        view:append_text(output)
      end
    else
      if output == "Exit code: 0" or output == "" then
        view:clear()
        view:append({ { "No output", "dim" } })
      else
        view:append_text(output)
      end
    end
    view:finish()
    return buf
  end,

  handler = function(input, ctx)
    if not input.command then
      return { llm_output = "error: command is required", is_error = true }
    end
    if input.tail and input.tail < 1 then
      return { llm_output = "error: tail must be >= 1", is_error = true }
    end

    local timeout_secs = input.timeout or opts.timeout_secs
    local max_lines, max_bytes = output_limits.resolve(opts, ctx)

    ctx:set_deadline(timeout_secs + DEADLINE_GRACE_SECS)

    local session = ctx:session_id()
    if not session then
      return { llm_output = "error: pwsh needs a session", is_error = true }
    end

    local buf, view = create_pwsh_view(input.command, ctx)
    view:append({ { "Waiting for output...", "dim" } })

    if input.reset then
      maki.pwsh.reset({ session = session })
    end

    local r, err = maki.pwsh.exec({
      session = session,
      command = input.command,
      timeout = timeout_secs,
    })
    if not r then
      view:clear()
      view:append({ { "Error: " .. err, "dim" } })
      view:finish()
      return { llm_output = "error: " .. err, is_error = true }
    end

    local output = r.output or ""
    if input.tail then
      output = output_limits.tail(output, input.tail)
    end
    output = truncate(output, max_lines, max_bytes)

    local is_error = (r.exit_code or 0) ~= 0 or r.timed_out or r.dead
    local llm_output
    if r.timed_out then
      llm_output = (output .. "\ntool pwsh timed out after " .. timeout_secs .. "s; the persistent session was killed"):gsub(
        "^\n",
        ""
      )
    elseif r.dead then
      llm_output = (output .. "\nthe persistent session died; the next call starts a fresh one"):gsub("^\n", "")
    elseif is_error then
      llm_output = output == "" and ("Exit code: " .. r.exit_code) or (output .. "\nExit code: " .. r.exit_code)
    else
      llm_output = output == "" and "Exit code: 0" or output
    end

    if output == "" and not r.timed_out and not r.dead then
      view:clear()
      view:append({ { "No output", "dim" } })
    end
    if (r.exit_code or 0) ~= 0 then
      view:append({ { "Exit code: " .. r.exit_code, "dim" } })
    end
    view:finish()

    return { llm_output = llm_output, is_error = is_error }
  end,
})
