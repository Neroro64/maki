local Preview = require("design_preview")
local Artifacts = require("design_artifacts")
local th = require("maki.test_helpers")
local case, eq = th.case, th.eq

local URL = "http://localhost:3000"
local DIR = "/repo/.maki/design"
local COMMAND = "npm run dev"
local SESSION = "design-session"
local LOG_ERROR = "server could not bind"

local function harness(body)
  local saved = {}
  local h = { files = {}, starts = {}, stops = {}, hooks = {}, probes = {}, running = false, changes = 0, time = 0 }
  local function replace(table, key, value)
    saved[#saved + 1] = { table, key, table[key] }
    table[key] = value
  end
  replace(maki.uv, "cwd", function()
    return "/repo"
  end)
  replace(maki.fs, "joinpath", function(...)
    return table.concat({ ... }, "/")
  end)
  replace(maki.fs, "metadata", function(path)
    return h.files[path] and { is_file = true } or nil
  end)
  replace(maki.fs, "read", function(path)
    return h.files[path]
  end)
  replace(maki.fs, "mkdir", function()
    return true
  end)
  replace(maki.fs, "append", function(path, text)
    h.files[path] = (h.files[path] or "") .. text
    return true
  end)
  replace(maki.fs, "write", function(path, text)
    if h.write_error then
      return nil, h.write_error
    end
    h.files[path] = text
    return true
  end)
  replace(maki.browser, "preview_status", function(opts)
    eq(opts.timeout, 1)
    local response = table.remove(h.probes, 1)
    if type(response) == "number" then
      return response
    end
    return nil, response or "connection refused"
  end)
  replace(maki.fn, "jobstart", function(command, opts)
    h.starts[#h.starts + 1] = { command = command, opts = opts }
    if h.start_error then
      return nil, h.start_error
    end
    h.running = true
    return #h.starts
  end)
  replace(maki.fn, "jobinfo", function()
    return { status = h.running and "running" or "exited" }
  end)
  replace(maki.fn, "jobstop", function(id)
    h.stops[#h.stops + 1] = id
    h.running = false
  end)
  replace(maki.async, "on_cancel", function(fn)
    h.hooks[#h.hooks + 1] = fn
  end)
  replace(maki.async, "sleep", function()
    h.time += 1
    if h.sleep then
      h.sleep()
    end
  end)
  h.preview = Preview.new({ preview_port = 3000, preview_timeout = 3 }, function()
    h.changes += 1
  end, function()
    return h.time
  end)
  local ok, err = pcall(body, h)
  for i = #saved, 1, -1 do
    local entry = saved[i]
    entry[1][entry[2]] = entry[3]
  end
  if not ok then
    error(err, 0)
  end
end

case("preview_reuses_external_server_without_owning_or_stopping_it", function()
  harness(function(h)
    h.probes = { 200 }
    local result = h.preview:start({}, SESSION, DIR)
    eq(result.is_error, nil)
    eq(h.preview.url, URL)
    eq(#h.starts, 0)
    assert(result.llm_output:find("external server", 1, true))
    eq(h.preview:stop(), false)
    eq(#h.stops, 0)
  end)
end)

case("explicit_command_cannot_claim_an_occupied_url", function()
  harness(function(h)
    h.probes = { 200 }
    local result = h.preview:start({ command = COMMAND }, SESSION, DIR)
    eq(result.is_error, true)
    assert(result.llm_output:find("already answers", 1, true))
    eq(#h.starts, 0)
  end)
end)

case("preview_waits_through_server_error_and_then_reuses_the_owned_process", function()
  harness(function(h)
    h.probes = { "connection refused", 503, 200 }
    local result = h.preview:start({ command = COMMAND }, SESSION, DIR)
    eq(result.is_error, nil)
    eq(h.preview.job_id, 1)
    eq(h.preview.owner, SESSION)
    eq(h.starts[1].opts.scope, "plugin")
    eq(h.starts[1].opts.stdout, DIR .. "/preview.log")
    eq(h.starts[1].opts.stderr, DIR .. "/preview.log")
    eq(h.starts[1].opts.cwd, "/repo")
    assert(result.llm_output:find("view_image", 1, true))
    h.probes = { 200 }
    result = h.preview:start({}, SESSION, DIR)
    eq(result.is_error, nil)
    eq(#h.starts, 1)
    eq(#h.stops, 0)
    for _, hook in ipairs(h.hooks) do
      hook()
    end
    eq(#h.stops, 0, "cancelling after readiness must not stop the persistent preview")
    eq(h.preview:stop(), true)
    eq(h.stops[1], 1)
  end)
end)

case("preview_changing_route_keeps_owned_process_managed", function()
  harness(function(h)
    h.probes = { "connection refused", 200 }
    h.preview:start({ command = COMMAND }, SESSION, DIR)
    h.probes = { 200 }
    h.preview:start({ url = URL .. "/settings" }, SESSION, DIR)
    eq(h.preview.url, URL .. "/settings")
    eq(h.preview.job_id, 1)
    eq(#h.starts, 1)
    eq(#h.stops, 0)
  end)
end)

case("startup_timeout_stops_process_and_returns_error_log", function()
  harness(function(h)
    h.probes = { "connection refused", 503, 503, 503 }
    h.sleep = function()
      h.files[DIR .. "/preview.log"] = LOG_ERROR
    end
    local result = h.preview:start({ command = COMMAND }, SESSION, DIR)
    eq(result.is_error, true)
    eq(h.stops[1], 1)
    eq(h.preview.url, nil)
    assert(result.llm_output:find(LOG_ERROR, 1, true))
    assert(result.llm_output:find("timed out", 1, true))
  end)
end)

case("startup_cancellation_stops_process_before_handler_unwinds", function()
  harness(function(h)
    h.probes = { "connection refused", "connection refused" }
    h.sleep = function()
      for _, hook in ipairs(h.hooks) do
        hook()
      end
    end
    local result = h.preview:start({ command = COMMAND }, SESSION, DIR)
    eq(result.is_error, true)
    eq(#h.stops, 1)
    eq(h.preview.job_id, nil)
  end)
end)

case("server_exit_callback_clears_live_url_and_status", function()
  harness(function(h)
    h.probes = { "connection refused", 200 }
    h.preview:start({ command = COMMAND }, SESSION, DIR)
    local before = h.changes
    h.starts[1].opts.on_exit(1)
    eq(h.preview.job_id, nil)
    eq(h.preview.url, nil)
    eq(h.changes, before + 1)
  end)
end)

case("malformed_preview_url_never_starts_a_process", function()
  harness(function(h)
    h.probes = { "preview URL must use http:// with localhost or a loopback IP and no credentials" }
    local result = h.preview:start({ command = COMMAND, url = "https://example.com" }, SESSION, DIR)
    eq(result.is_error, true)
    eq(#h.starts, 0)
  end)
end)

case("npm_detection_uses_fixed_script_priority", function()
  harness(function(h)
    h.files["/repo/package.json"] = maki.json.encode({ scripts = { serve = "serve", start = "start", dev = "dev" } })
    h.probes = { "connection refused", 200 }
    local result = h.preview:start({}, SESSION, DIR)
    eq(result.is_error, nil)
    eq(h.starts[1].command, COMMAND)
  end)
end)

case("viewport_matrix_is_persisted_and_custom_sizes_survive_readback", function()
  harness(function(h)
    local defaults = Artifacts.viewports()
    eq(#defaults, 3)
    eq(defaults[1].width, 390)
    assert(h.files[DIR .. "/viewports.json"])
    local custom = { { name = "small", width = 320, height = 640, mobile = true } }
    assert(Artifacts.viewports(custom))
    local matrix = Artifacts.viewports()
    eq(#matrix, 1)
    eq(matrix[1].width, 320)
    eq(matrix[1].mobile, true)
  end)
end)

case("invalid_saved_matrix_is_reported_instead_of_silently_replaced", function()
  harness(function(h)
    local malformed = "{invalid"
    h.files[DIR .. "/viewports.json"] = malformed
    local matrix, err = Artifacts.viewports()
    eq(matrix, nil)
    assert(err)
    eq(h.files[DIR .. "/viewports.json"], malformed)
  end)
end)

case("viewport_validation_rejects_duplicate_names_and_out_of_range_sizes", function()
  for _, matrix in ipairs({
    {},
    { { name = "mobile", width = 0, height = 844 } },
    { { name = "mobile", width = 390.5, height = 844 } },
    { { name = "mobile", width = 390, height = 16385 } },
    { { name = "../outside", width = 390, height = 844 } },
    { { name = "mobile", width = 390, height = 844, mobile = "yes" } },
    { { name = "same", width = 390, height = 844 }, { name = "same", width = 834, height = 1112 } },
  }) do
    local valid, err = Artifacts.validate_viewports(matrix)
    eq(valid, nil)
    assert(err)
  end
end)

case("notes_append_preserves_earlier_feedback_and_reports_write_failure", function()
  harness(function(h)
    assert(Artifacts.notes("larger buttons", "user"))
    assert(Artifacts.notes("keep the brand color", "designer"))
    local notes = Artifacts.notes()
    assert(notes:find("user: larger buttons", 1, true))
    assert(notes:find("designer: keep the brand color", 1, true))
    h.write_error = "disk full"
    local matrix, err = Artifacts.viewports({ { name = "mobile", width = 390, height = 844 } })
    eq(matrix, nil)
    eq(err, h.write_error)
  end)
end)

th.report()
