local output_limits = require("maki.output_limits")

local M = {}

--- Keep the original capture whenever presentation hides output. Interrupted
--- commands also retain their partial capture, even when it fits on screen.
function M.render(text, max_lines, max_bytes, tail, interrupted)
  local output = tail and output_limits.tail(text, tail) or text
  output = output_limits.cut_middle(output, max_lines, max_bytes)
  local reduced = output ~= text
  if reduced or (interrupted and text ~= "") then
    local path, err = maki.fn.retain_output(text)
    if path then
      output = output .. (output ~= "" and "\n" or "") .. "Full output: " .. path
    else
      output = text .. (text ~= "" and "\n" or "") .. "[output retention failed: " .. tostring(err) .. "; showing full capture]"
    end
  end
  return output, reduced
end

function M.exited(output, code, reduced)
  if code ~= 0 or reduced or output == "" then
    return output .. (output ~= "" and "\n" or "") .. "Exit code: " .. code
  end
  return output
end

return M
