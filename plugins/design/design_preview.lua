local Preview = {}
Preview.__index = Preview

local POLL_MS = 150
local LOG_TAIL_BYTES = 12000

function Preview.new(config, changed, clock)
  return setmetatable({ config = config, changed = changed, clock = clock or os.time }, Preview)
end

function Preview:alive()
  if not self.job_id then
    return false
  end
  local info = maki.fn.jobinfo(self.job_id)
  return info ~= nil and info.status == "running"
end

function Preview:stop()
  local owned = self.job_id ~= nil
  if self.job_id then
    maki.fn.jobstop(self.job_id)
  end
  self.job_id, self.url, self.command, self.owner = nil, nil, nil, nil
  self.changed()
  return owned
end

local function detected_command(cwd)
  local content = maki.fs.read(maki.fs.joinpath(cwd, "package.json"))
  if not content then
    return nil
  end
  local package = maki.json.decode(content)
  if type(package) ~= "table" or type(package.scripts) ~= "table" then
    return nil
  end
  for _, name in ipairs({ "dev", "start", "serve" }) do
    if type(package.scripts[name]) == "string" and package.scripts[name] ~= "" then
      return "npm run " .. name
    end
  end
end

local function failure(message, log)
  local body = log and maki.fs.read(log, { offset = -80 }) or ""
  if body and #body > LOG_TAIL_BYTES then
    body = "[last " .. LOG_TAIL_BYTES .. " bytes]\n" .. body:sub(-LOG_TAIL_BYTES)
  end
  return {
    llm_output = "error: " .. message .. (log and ("\nlog: " .. log .. "\n" .. (body or "")) or ""),
    is_error = true,
  }
end

function Preview:start(input, session, dir)
  local cwd = maki.uv.cwd()
  if not cwd then
    return failure("cannot resolve the project working directory")
  end
  local url = input.url or ("http://localhost:" .. tostring(input.port or self.config.preview_port))
  if self:alive() and not input.command and not input.url and not input.port then
    url = self.url
  end
  local status, probe_err = maki.browser.preview_status({ url = url, timeout = 1 })
  if probe_err and probe_err:find("preview URL", 1, true) then
    return failure(probe_err)
  end
  local same_origin = self.url and self.url:match("^http://[^/]+") == url:match("^http://[^/]+")
  local same = self:alive() and same_origin and (not input.command or input.command == self.command)
  if same and status and status < 500 then
    self.url = url
    self.changed()
    return { llm_output = self:report(status, dir, "Reusing the managed preview") }
  end
  if status and not same then
    if status >= 500 then
      return failure("existing preview at " .. url .. " returns HTTP " .. status)
    end
    if input.command then
      return failure("another server already answers at " .. url .. "; omit command to reuse it")
    end
    self:stop()
    self.url, self.owner = url, session
    self.changed()
    return { llm_output = self:report(status, dir, "Reusing an external server (not managed or stopped by Maki)") }
  end
  if self.job_id then
    self:stop()
  end
  local command = input.command or detected_command(cwd)
  if not command or command:match("^%s*$") then
    return failure(
      "no npm dev/start/serve script found and no preview answers at "
        .. url
        .. ". Pass command and url, or start the application yourself. Native desktop applications use computer, not a web preview."
    )
  end
  local ok, err = maki.fs.mkdir(dir, { parents = true })
  if not ok then
    return failure(err)
  end
  local log = maki.fs.joinpath(dir, "preview.log")
  local _, log_err = maki.fs.append(log, "\n[" .. os.date("%Y-%m-%d %H:%M:%S") .. "] " .. command .. "\n")
  if log_err then
    return failure(log_err)
  end
  local id, start_err = maki.fn.jobstart(command, {
    cwd = cwd,
    stdout = log,
    stderr = log,
    scope = "plugin",
    name = "design-preview",
    on_exit = function(job_id)
      if self.job_id == job_id then
        self.job_id, self.url, self.owner = nil, nil, nil
        self.changed()
      end
    end,
  })
  if not id then
    return failure("cannot start " .. command .. ": " .. tostring(start_err), log)
  end
  self.job_id, self.command, self.url, self.owner, self.log = id, command, url, session, log
  local ready = false
  maki.async.on_cancel(function()
    if not ready and self.job_id == id then
      self:stop()
    end
  end)
  local deadline = self.clock() + self.config.preview_timeout
  local last_error = probe_err
  while self.clock() < deadline do
    if not self:alive() then
      self:stop()
      return failure("dev server exited before becoming ready: " .. command, log)
    end
    status, probe_err = maki.browser.preview_status({ url = url, timeout = 1 })
    if status and status < 500 and self:alive() then
      ready = true
      self.changed()
      return { llm_output = self:report(status, dir, "Dev server ready"), written_path = log }
    end
    last_error = probe_err or ("HTTP " .. tostring(status))
    maki.async.sleep(POLL_MS)
  end
  self:stop()
  return failure("timed out waiting for " .. url .. ": " .. tostring(last_error), log)
end

function Preview:report(status, dir, prefix)
  return prefix
    .. " at "
    .. self.url
    .. " (HTTP "
    .. tostring(status)
    .. ").\n"
    .. "An HTTP response does not prove that the rendered page is correct. Open it with browser, screenshot, then inspect with view_image.\n"
    .. "Tell the user to open "
    .. self.url
    .. " to watch live. Artifacts: "
    .. dir
    .. (self.log and ("\nDev-server log: " .. self.log) or "")
end

return Preview
