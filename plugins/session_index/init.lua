-- Session recall adapted from maki-session-index, MIT, Khaled Jaafar (2026).
-- Native storage readers own log versions, rewrites and partial records.
local truncate = require("maki.truncate")
local output_limits = require("maki.output_limits")

local function answer(text, err, ctx)
  if not text then
    return { llm_output = "error: " .. err, is_error = true }
  end
  local max_lines, max_bytes = output_limits.resolve({}, ctx)
  return truncate(text, max_lines, max_bytes)
end

maki.api.register_tool({
  name = "session_index",
  kind = "search",
  description = [[Search stored chat sessions without opening or resuming them.

- Case-insensitive literal search over full whitespace-collapsed text, with bounded windows around hits.
- Thinking and images are excluded. Tool calls show compact targets. Results show only [ok] or a short error marker.
- Omit query to list recent sessions. Scope defaults to the current working directory. Use all for every project.
- Follow up with session_read and an id from a hit. Searches current saved logs, not archives or unsaved live messages.
- At most 100 rows and 24 KiB per answer.]],
  schema = {
    type = "object",
    properties = {
      query = { type = "string", description = "Literal text to find, case-insensitive. Omit to list recent sessions." },
      scope = { type = "string", enum = { "cwd", "all" }, description = "Project scope: cwd (default) or all." },
      limit = {
        type = "integer",
        minimum = 1,
        maximum = 100,
        description = "Max rows, default 20 for search or 30 when listing.",
      },
    },
  },
  handler = function(input, ctx)
    local text, err = maki.session.index(input)
    return answer(text, err, ctx)
  end,
})

maki.api.register_tool({
  name = "session_read",
  kind = "read",
  description = [[Read the newest normalized messages of a stored session, without opening or resuming it.

- Use an exact id or unique case-sensitive prefix from session_index. Ambiguous prefixes are rejected.
- Reads across projects. Thinking is excluded and tool-result bodies are scrubbed.
- Each message is capped at 4 KiB and the answer at 24 KiB. Newest messages take priority when the cap is reached.]],
  schema = {
    type = "object",
    properties = {
      id = { type = "string", description = "Stored session id or unique prefix." },
      last = { type = "integer", minimum = 1, maximum = 100, description = "Newest message count, default 4." },
    },
    required = { "id" },
  },
  handler = function(input, ctx)
    local text, err = maki.session.recall(input)
    return answer(text, err, ctx)
  end,
})
