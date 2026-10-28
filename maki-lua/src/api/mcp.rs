//! `maki.mcp`: read-only views over the MCP servers this session runs with.

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Value};
use tokio::sync::mpsc::UnboundedSender;

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
async fn servers(lua: Lua, #[ctx] tx: Option<UnboundedSender<UiAction>>) -> LuaResult<Pair<Value>> {
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
    "maki.mcp" => pub(crate) fn create_mcp_table(tx: Option<UnboundedSender<UiAction>>),
    DOCS [servers(tx)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, UiReply};
    use serde_json::json;
    use std::thread::JoinHandle;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    const SERVERS_CALL: &str = "return mcp.servers()";
    const NO_REQUEST_ERR: &str = "expected an mcp request";
    const NAME: &str = "github";
    const STATUS: &str = "needs_auth";

    fn lua_with_mcp(tx: Option<UnboundedSender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_mcp_table(&lua, tx).unwrap();
        lua.globals().set("mcp", t).unwrap();
        lua
    }

    fn spawn_host(mut rx: UnboundedReceiver<UiAction>, reply: UiReply) -> JoinHandle<McpRequest> {
        std::thread::spawn(move || {
            let Some(UiAction::Mcp { req, reply_tx }) = rx.blocking_recv() else {
                panic!("{NO_REQUEST_ERR}");
            };
            reply_tx.send(reply).unwrap();
            req
        })
    }

    #[tokio::test]
    async fn without_ui_returns_error_pair() {
        let lua = lua_with_mcp(None);
        let (val, err): (Value, Option<String>) = lua.load(SERVERS_CALL).eval_async().await.unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[tokio::test]
    async fn servers_pass_the_host_array_through() {
        let (tx, rx) = unbounded_channel::<UiAction>();
        let lua = lua_with_mcp(Some(tx));
        let host = spawn_host(
            rx,
            Ok(json!([{ "name": NAME, "status": STATUS, "needs_auth": true }])),
        );

        let servers: mlua::Table = lua.load(SERVERS_CALL).eval_async().await.unwrap();
        let first: mlua::Table = servers.get(1).unwrap();
        assert_eq!(first.get::<String>("name").unwrap(), NAME);
        assert_eq!(first.get::<String>("status").unwrap(), STATUS);
        assert!(first.get::<bool>("needs_auth").unwrap());

        drop(lua);
        assert!(matches!(host.join().unwrap(), McpRequest::Servers));
    }
}
