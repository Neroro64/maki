local M = {}

local DEFAULT_VIEWPORTS = {
  { name = "mobile", width = 390, height = 844, mobile = true },
  { name = "tablet", width = 834, height = 1112, mobile = false },
  { name = "desktop", width = 1440, height = 900, mobile = false },
}
local MAX_VIEWPORTS = 12
local MAX_DIMENSION = 16384

function M.dir()
  return maki.fs.joinpath(maki.uv.cwd() or ".", ".maki", "design")
end

local function read_optional(path)
  local meta, err = maki.fs.metadata(path)
  if err then
    return nil, err
  end
  if not meta then
    return ""
  end
  return maki.fs.read(path)
end

function M.validate_viewports(matrix)
  if type(matrix) ~= "table" or #matrix == 0 or #matrix > MAX_VIEWPORTS then
    return nil, "viewports must contain between 1 and 12 entries"
  end
  local names = {}
  for _, viewport in ipairs(matrix) do
    if type(viewport) ~= "table" or type(viewport.name) ~= "string" or not viewport.name:match("^[%w_-]+$") then
      return nil, "each viewport needs a name containing letters, digits, underscores or hyphens"
    end
    if names[viewport.name] then
      return nil, "duplicate viewport name: " .. viewport.name
    end
    names[viewport.name] = true
    for _, key in ipairs({ "width", "height" }) do
      local value = viewport[key]
      if type(value) ~= "number" or value % 1 ~= 0 or value < 1 or value > MAX_DIMENSION then
        return nil, "viewport width and height must be integers in 1..=16384"
      end
    end
    if viewport.mobile ~= nil and type(viewport.mobile) ~= "boolean" then
      return nil, "viewport mobile must be a boolean"
    end
  end
  return matrix
end

function M.viewports(replacement)
  local path = maki.fs.joinpath(M.dir(), "viewports.json")
  local matrix = replacement
  local save = replacement ~= nil
  if not matrix then
    local content, err = read_optional(path)
    if not content then
      return nil, err
    end
    if content == "" then
      matrix, save = DEFAULT_VIEWPORTS, true
    else
      local decode_err
      matrix, decode_err = maki.json.decode(content)
      if not matrix then
        return nil, "cannot read " .. path .. ": " .. tostring(decode_err)
      end
    end
  end
  local valid, err = M.validate_viewports(matrix)
  if not valid then
    return nil, err
  end
  if save then
    local ok, mkdir_err = maki.fs.mkdir(M.dir(), { parents = true })
    if not ok then
      return nil, mkdir_err
    end
    local written, write_err = maki.fs.write(path, maki.json.encode(matrix) .. "\n")
    if not written then
      return nil, write_err
    end
  end
  return matrix
end

function M.notes(text, author)
  local path = maki.fs.joinpath(M.dir(), "notes.md")
  if text and text:match("%S") then
    local ok, err = maki.fs.mkdir(M.dir(), { parents = true })
    if not ok then
      return nil, err
    end
    local written, write_err =
      maki.fs.append(path, string.format("- [%s] %s: %s\n", os.date("%Y-%m-%d %H:%M"), author or "user", text))
    if not written then
      return nil, write_err
    end
    return "Noted: " .. text
  end
  return read_optional(path)
end

function M.report(preview)
  local dir = M.dir()
  local lines = {
    "Design artifacts: " .. dir,
    preview.url and ("Preview URL: " .. preview.url) or "Preview: not running",
    preview.job_id and ("Managed process: " .. tostring(preview.job_id))
      or "No managed preview process. External servers are not stopped by Maki.",
    "Dev-server log: " .. maki.fs.joinpath(dir, "preview.log"),
    "Viewport matrix: " .. maki.fs.joinpath(dir, "viewports.json"),
    "Notes: " .. maki.fs.joinpath(dir, "notes.md"),
  }
  local matrix, matrix_err = M.viewports()
  if matrix then
    for _, viewport in ipairs(matrix) do
      lines[#lines + 1] = string.format("  %s: %dx%d", viewport.name, viewport.width, viewport.height)
    end
  else
    lines[#lines + 1] = "Cannot read viewport matrix: " .. tostring(matrix_err)
  end
  local notes, err = M.notes()
  if err then
    lines[#lines + 1] = "Cannot read notes: " .. err
  elseif notes ~= "" then
    lines[#lines + 1] = notes
  end
  local meta = maki.fs.metadata(dir)
  local entries, list_err
  if meta then
    entries, list_err = maki.fs.dir(dir, { depth = 3 })
  end
  lines[#lines + 1] = "Saved screenshots (inspect with view_image):"
  local screenshots = {}
  for _, entry in ipairs(entries or {}) do
    if entry[2] == "file" and entry[1]:lower():match("%.png$") then
      screenshots[#screenshots + 1] = maki.fs.joinpath(dir, entry[1])
    end
  end
  table.sort(screenshots)
  for _, path in ipairs(screenshots) do
    lines[#lines + 1] = path
  end
  if #screenshots == 0 then
    lines[#lines + 1] = list_err and ("Cannot list screenshots: " .. list_err) or "No PNG screenshots saved here yet."
  end
  return table.concat(lines, "\n")
end

return M
