local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
local command_output = require("maki.command_output")
local partial = require("maki.partial")

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

The PowerShell process stays alive for the whole agent session: variables, functions, and the working directory carry over between calls. Use it on Windows and for PowerShell-specific work (cmdlets, $variables, object pipelines). Use bash for POSIX shell commands.

- Read, list, search, and edit files with dedicated tools, not PowerShell cmdlets.
- State persists: set a variable in one call, read it in the next.
- Concurrent calls in the same session run one at a time. Their timeout includes queued time.
- Multiline commands and here-strings are supported, including blank lines.
- PowerShell errors and nonzero native exit codes are reported as failures. Ordinary errors leave the session usable.
- A running command that times out kills the process. The next call starts fresh with no state. A timeout while queued leaves the running command alone.
- Output beyond 2000 lines or 50KB, or hidden by `tail`, is retained in the reported `Full output` log. Read that capture instead of rerunning.
- Run builds/tests once, unfiltered (no shell tail/filter). Use the tool's `tail` for a bounded view; use a test logger/result artifact for diagnostics the runner does not print.
- Provide a short `description` (3-5 words).
- Use the `tail` param to return the last N output lines without changing the command.]]

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use pwsh on Windows and for PowerShell-specific work; use bash for POSIX shell commands. pwsh keeps one process alive per session, so variables and cwd persist across calls.",
})

local opts = maki.api.register_options(output_limits.extend({
  timeout_secs = {
    default = 120,
    min = 5,
    desc = "Maximum seconds per call, including queued time. A running command that times out kills the session. A call's `timeout` param overrides it.",
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
      timeout = { type = "integer", description = "Timeout in seconds, including queued time (default 120)" },
      tail = { type = "integer", description = "Return only the last N output lines, retaining the full capture" },
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

    local started_at = os.time()
    local timeout_secs = input.timeout or opts.timeout_secs
    local max_lines, max_bytes = output_limits.resolve(opts, ctx)

    ctx:set_deadline(timeout_secs + DEADLINE_GRACE_SECS)

    local session = ctx:session_id()
    if not session then
      return { llm_output = "error: pwsh needs a session", is_error = true }
    end

    local buf, view = create_pwsh_view(input.command, ctx)
    local output_parts = {}
    local has_output = false
    local finished = false
    local function final_output(interrupted)
      return command_output.render(table.concat(output_parts, "\n"), max_lines, max_bytes, input.tail, interrupted)
    end

    maki.async.on_cancel(function(reason)
      if finished then
        return
      end
      finished = true
      ctx:finish(partial.cut(view, final_output(true), reason, timeout_secs, started_at))
    end)
    view:append({ { "Waiting for output...", "dim" } })
    ctx:live_buf(buf)

    if input.reset then
      maki.pwsh.reset({ session = session })
    end

    local r, err = maki.pwsh.exec({
      session = session,
      command = input.command,
      timeout = timeout_secs,
      collect_output = false,
      on_output = function(line)
        if finished then
          return
        end
        if not has_output then
          has_output = true
          view:clear()
        end
        output_parts[#output_parts + 1] = line
        view:append(line)
      end,
    })
    finished = true
    if not r then
      local output = final_output(true)
      local message = "error: " .. err
      if output == "" then
        view:clear()
      end
      view:append({ { message, "dim" } })
      view:finish()
      return { llm_output = output .. (output ~= "" and "\n" or "") .. message, is_error = true, body = buf }
    end

    if not r.timed_out and not r.dead and output_parts[#output_parts] == "" then
      table.remove(output_parts)
    end
    if #output_parts == 0 and not r.timed_out and not r.dead then
      view:clear()
      view:append({ { "No output", "dim" } })
    end
    local output, reduced = final_output(r.timed_out or r.dead)
    local is_error = r.exit_code ~= 0 or r.timed_out or r.dead
    local llm_output
    if r.timed_out then
      llm_output = output .. (output ~= "" and "\n" or "")
        .. "tool pwsh timed out after " .. timeout_secs .. "s; the persistent session was killed"
    elseif r.dead then
      llm_output = output .. (output ~= "" and "\n" or "")
        .. "the persistent session died; the next call starts a fresh one"
    elseif r.exit_code == nil then
      llm_output = output .. (output ~= "" and "\n" or "") .. "Exit code unavailable"
    else
      llm_output = command_output.exited(output, r.exit_code, reduced)
    end

    if r.timed_out then
      view:append({ { "Timed out; the persistent session was killed", "dim" } })
    elseif r.dead then
      view:append({ { "The persistent session died", "dim" } })
    end
    if (r.exit_code or 0) ~= 0 then
      view:append({ { "Exit code: " .. r.exit_code, "dim" } })
    end
    view:finish()

    return { llm_output = llm_output, is_error = is_error, body = buf }
  end,
})
