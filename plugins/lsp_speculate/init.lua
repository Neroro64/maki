local opts = maki.api.register_options({
  rust_command = { default = "rust-analyzer", desc = "Rust LSP command, shell-words quoted; executed directly without a shell." },
  python_command = { default = "pyright-langserver --stdio", desc = "Python LSP command, shell-words quoted; executed directly without a shell." },
  typescript_command = { default = "typescript-language-server --stdio", desc = "TypeScript/TSX LSP command (not raw tsserver)." },
  javascript_command = { default = "typescript-language-server --stdio", desc = "JavaScript/JSX LSP command (not raw tsserver)." },
  go_command = { default = "gopls", desc = "Go LSP command, shell-words quoted; executed directly without a shell." },
  command = { default = "", desc = "Override the LSP command for every file; quote argv with shell-words syntax, never shell syntax." },
  language_id = { default = "", desc = "Override the LSP language id; empty infers it from the file extension." },
  root = { default = "", desc = "Workspace root; empty uses the nearest .git ancestor or file directory." },
  timeout_ms = { default = 30000, min = 1, desc = "Total speculation deadline in milliseconds." },
})

local LANGUAGES = {
  rs = "rust",
  py = "python",
  pyi = "python",
  js = "javascript",
  mjs = "javascript",
  cjs = "javascript",
  jsx = "javascript",
  ts = "typescript",
  mts = "typescript",
  cts = "typescript",
  tsx = "typescript",
  go = "go",
}

local function configured(value)
  return value ~= "" and value or nil
end

local function server_command(file_path)
  if opts.command ~= "" then
    return opts.command
  end
  local language = configured(opts.language_id) or LANGUAGES[file_path:match("%.([^./\\]+)$")]
  if language == "typescriptreact" then
    language = "typescript"
  elseif language == "javascriptreact" then
    language = "javascript"
  end
  return language and opts[language .. "_command"] or nil
end

maki.api.register_tool({
  name = "lsp_speculate",
  kind = "read",
  permission = "fs_read",
  permission_scopes = "file_path",
  audiences = { "main", "research_sub", "general_sub", "interpreter" },
  description = [[Diagnose a candidate line replacement with a real language server, entirely in memory. Never writes the file.

start_line and end_line are inclusive, 1-based source lines. replacement_text replaces those whole lines; an empty string deletes them. Each trial starts from the current file on disk, then restores that source in the server and closes the document.

Returns JSON {file_path, diagnostics, clean}. Diagnostics include file_path, 1-based line/end_line, severity, the original server message, and optional source/code. Unrelated baseline diagnostics are excluded. Errors or warnings touching the replacement and newly reported elsewhere are retained. clean=true means no retained LSP errors or warnings, not that tests passed. Missing servers, failed diagnostics, and timeouts are errors, never clean results.

Supports Rust (rust-analyzer), Python (pyright-langserver --stdio), JS/TS (typescript-language-server --stdio, not raw tsserver), and Go (gopls). Servers must be installed. Server argv, language id, workspace root, and timeout are trusted plugin configuration, not model inputs.

Use code_execution to explore candidates without writing between trials:

import json
for candidate in candidates:
    result = json.loads(await lsp_speculate(file_path="src/main.rs", start_line=3, end_line=3, replacement_text=candidate))
    if result["clean"]:
        print(candidate)
        break

Only apply the selected candidate afterward with an edit tool.]],
  schema = {
    type = "object",
    additionalProperties = false,
    properties = {
      file_path = { type = "string", description = "Existing source file, absolute, cwd-relative, or ~/ path.", required = true },
      start_line = { type = "integer", minimum = 1, description = "First source line to replace (inclusive, 1-based).", required = true },
      end_line = { type = "integer", minimum = 1, description = "Last source line to replace (inclusive, 1-based).", required = true },
      replacement_text = { type = "string", description = "Whole-line candidate replacement; empty deletes the range.", required = true },
    },
  },
  examples = {
    { file_path = "src/main.rs", start_line = 3, end_line = 3, replacement_text = "    let answer: i32 = 42;" },
  },
  handler = function(input)
    if type(input.file_path) ~= "string" or input.file_path == "" then
      return { llm_output = "lsp_speculate error: file_path is required", is_error = true }
    end
    local result, err = maki.lsp.speculate({
      file_path = input.file_path,
      start_line = input.start_line,
      end_line = input.end_line,
      replacement_text = input.replacement_text,
      command = server_command(input.file_path),
      language_id = configured(opts.language_id),
      root = configured(opts.root),
      timeout = opts.timeout_ms,
    })
    if not result then
      return { llm_output = "lsp_speculate error: " .. tostring(err), is_error = true }
    end
    return maki.json.encode(result)
  end,
})
