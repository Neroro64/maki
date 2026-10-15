local NO_NOTES = "No context notes are stored for this session."
local SAVED = "Context notes saved."

local DESCRIPTION =
  [[Read or replace the persistent context notebook for this session. Omit text to read; supply text to replace the whole notebook; empty string clears it. The notebook survives compaction and is re-injected into context. Keep it a compact index: task state, decisions, changed files, evidence, blockers, next steps — one line per item, no copied logs or file bodies. Cap 16 KiB.]]

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Near the compaction threshold, context_notes saves a durable notebook that survives compaction; keep it a compact index.",
})

maki.api.register_tool({
  name = "context_notes",
  kind = "read",
  description = DESCRIPTION,
  schema = {
    type = "object",
    properties = {
      text = {
        type = "string",
        description = 'The whole new notebook. Omit to read the current one, pass "" to clear it',
      },
    },
  },
  handler = function(input, ctx)
    local notes, err = maki.session.notes({ session = ctx:session_id(), text = input.text })
    if not notes then
      return { llm_output = "error: " .. tostring(err), is_error = true }
    end
    if input.text == nil then
      return { llm_output = notes.notes or NO_NOTES }
    end
    return { llm_output = SAVED }
  end,
})
