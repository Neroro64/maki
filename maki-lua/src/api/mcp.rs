//! `maki.mcp`: read-only views over the MCP servers this session runs with.

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Value};

use crate::api::util::command::{McpRequest, UiAction, ui_json_roundtrip};
use crate::api::util::pair::Pair;

/// The configured MCP servers, in the `/mcp` picker order. Each row carries
/// `name`, `status` (`"connecting"`, `"running"`, `"disabled"`, `"failed"`,
/// `"needs_auth"`) and `needs_auth`. A server that needs OAuth stays in
/// `"needs_auth"` until you run `/mcp auth <server>`; nothing opens a browser
/// on its own.
///
/// @return (table?, string?) Array of server tables, or `(nil, err)` when no
///   interactive UI is attached.
/// @example
/// for _, s in ipairs(maki.mcp.servers() or {}) do
///   print(s.name, s.status)
/// end
#[lua_fn]
async fn servers(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Mcp {
        req: McpRequest::Servers,
        reply_tx,
    })
    .await
}

lua_table! {
    /// Read-only views over the MCP servers this session runs with.
    /// Without an interactive UI every function returns
    /// `nil, "no interactive UI attached"`.
    "maki.mcp" => pub(crate) fn create_mcp_table(tx: Option<flume::Sender<UiAction>>),
    DOCS [servers(tx)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, UiReply};
    use serde_json::json;
    use std::thread::JoinHandle;

    const SERVERS_CALL: &str = "return mcp.servers()";
    const NO_REQUEST_ERR: &str = "expected an mcp request";
    const NAME: &str = "github";
    const STATUS: &str = "needs_auth";

    fn lua_with_mcp(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_mcp_table(&lua, tx).unwrap();
        lua.globals().set("mcp", t).unwrap();
        lua
    }

    fn spawn_host(rx: flume::Receiver<UiAction>, reply: UiReply) -> JoinHandle<McpRequest> {
        std::thread::spawn(move || {
            let Ok(UiAction::Mcp { req, reply_tx }) = rx.recv() else {
                panic!("{NO_REQUEST_ERR}");
            };
            reply_tx.send(reply).unwrap();
            req
        })
    }

    #[test]
    fn without_ui_returns_error_pair() {
        let lua = lua_with_mcp(None);
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load(SERVERS_CALL).eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[test]
    fn servers_pass_the_host_array_through() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_mcp(Some(tx));
        let host = spawn_host(
            rx,
            Ok(json!([{ "name": NAME, "status": STATUS, "needs_auth": true }])),
        );

        let servers: mlua::Table = smol::block_on(lua.load(SERVERS_CALL).eval_async()).unwrap();
        let first: mlua::Table = servers.get(1).unwrap();
        assert_eq!(first.get::<String>("name").unwrap(), NAME);
        assert_eq!(first.get::<String>("status").unwrap(), STATUS);
        assert!(first.get::<bool>("needs_auth").unwrap());

        drop(lua);
        assert!(matches!(host.join().unwrap(), McpRequest::Servers));
    }
}
