local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx_with_meta = helpers.idx_with_meta
local indexer = require("indexer")

case("core_file_eligibility_handles_extensions_and_exact_filenames", function()
  local cases = {
    { "/src/main.rs", "rust" },
    { "C:\\src\\main.rs", "rust" },
    { "/repo/BUILD.bazel", "bazel_build" },
    { "C:\\repo\\MODULE.bazel", "bazel_module" },
    { "/repo/Makefile", "make" },
    { "C:\\repo\\Dockerfile", "containerfile" },
    { "/repo/notes.md", "markdown" },
  }
  for _, c in ipairs(cases) do
    th.eq(indexer.language_for_path(c[1]), c[2])
  end
end)

case("core_unsupported_file_types_have_no_index_language", function()
  for _, path in ipairs({ "build.ps1", "C:\\repo\\module.psm1", "data.psd1", "notes.txt", "LICENSE", "MAIN.RS" }) do
    local lang = indexer.language_for_path(path)
    th.eq(lang, nil)
  end
  local skeleton, err = indexer.index_source("function Invoke-Build {}", "powershell")
  th.eq(skeleton, nil)
  assert(err ~= nil)
end)

local function meta_count(meta)
  local n = 0
  for _ in pairs(meta) do
    n = n + 1
  end
  return n
end

case("core_empty_and_trivial_source_returns_empty_meta", function()
  local cases = {
    { "", "empty" },
    { "   \n  \n\n", "whitespace-only" },
    { "// just a comment\n/* block comment */\n", "comments-only" },
  }
  for _, c in ipairs(cases) do
    local text, meta = idx_with_meta(c[1], "rust")
    assert(text == "", "expected empty text for " .. c[2] .. ", got: '" .. text .. "'")
    assert(meta_count(meta) == 0, "expected empty meta for " .. c[2])
  end
end)

case("core_meta_every_section_has_tag", function()
  local src = [[
use std::io;

const MAX: usize = 42;

pub struct Foo { x: i32 }

pub trait Bar { fn bar(&self); }

impl Foo { fn baz(&self) {} }

pub fn run() {}

pub mod utils;

macro_rules! m { () => {}; }

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
]]
  local text, meta = idx_with_meta(src, "rust")
  local lines = helpers.split_lines(text)
  local section_headers = {
    "imports:",
    "consts:",
    "types:",
    "traits:",
    "impls:",
    "fns:",
    "mod:",
    "macros:",
    "tests:",
  }
  for _, hdr in ipairs(section_headers) do
    local found = false
    for i, line in ipairs(lines) do
      if line:find(hdr, 1, true) then
        found = true
        local m = meta[i]
        assert(m, "section header '" .. hdr .. "' at line " .. i .. " has no meta")
        assert(
          m.tag == "section",
          "section header '" .. hdr .. "' tag is '" .. tostring(m.tag) .. "', expected 'section'"
        )
      end
    end
    assert(found, "section header '" .. hdr .. "' not found in output")
  end
end)
