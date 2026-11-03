local last_sig = nil

local function git_output(args)
  local id, err = maki.fn.jobstart(args)
  if not id then
    return nil, err
  end
  local res, wait_err = maki.fn.jobwait(id, 15000)
  if not res or res.exit_code ~= 0 then
    return nil, wait_err or (res and res.stderr)
  end
  return res.stdout or ""
end

maki.api.create_autocmd("TurnEnd", {
  callback = function()
    maki.async.run(function()
      local tracked = git_output({ "git", "diff", "--name-only", "HEAD" })
      local untracked = git_output({ "git", "ls-files", "--others", "--exclude-standard" })
      if not untracked then
        return
      end
      local out = (tracked or "") .. untracked
      local n = 0
      for _ in out:gmatch("[^\n]+") do
        n = n + 1
      end
      if n > 0 and out ~= last_sig then
        last_sig = out
        maki.ui.flash(n .. " file(s) changed — /review to inspect & comment")
      elseif n == 0 then
        last_sig = nil
      end
    end)
  end,
})
