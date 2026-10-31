local inspector = require("inspector")

maki.api.set_slot("agent.stop", function(prev, value, ctx)
  local previous = prev(value, ctx)
  if type(value) ~= "table" then
    return { continue = "[DIFF INTEGRITY WARNING] Malformed stop value; integrity cannot be verified.", required = true }
  end
  return inspector.gate(value, previous)
end)
