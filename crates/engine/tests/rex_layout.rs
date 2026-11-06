//! Opt-in against an explicitly disposable Rex server; creates and destroys a test session.
use flex_engine::{Connectivity, Engine};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn wait(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(12);
    while !ready() {
        assert!(Instant::now() < until, "Rex state did not converge");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn rpc(engine: &Engine, server: &str, method: &str, args: Value) -> Value {
    engine
        .shrew()
        .block_on(engine.call_server(server, method, args))
        .unwrap()
}
struct Cleanup<'a>(&'a Engine, String, String);
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = self.0.shrew().block_on(self.0.call_server(
            &self.1,
            "session.destroy",
            json!({"session_id":self.2}),
        ));
    }
}

#[test]
#[ignore = "requires isolated FLEX_TEST_SERVER; creates and destroys a temporary session"]
fn session_detached_block_and_layer_lifecycle() {
    let address = std::env::var("FLEX_TEST_SERVER").expect("explicit isolated server required");
    let engine = Engine::new(None, "flex-layout-test", Arc::new(|| {}), Arc::new(|_| {}));
    let server = engine.add_server("isolated", &address).unwrap();
    wait(|| {
        engine
            .state()
            .server(&server)
            .is_some_and(|s| s.connectivity == Connectivity::Online)
    });
    let created = rpc(
        &engine,
        &server,
        "session.create",
        json!({"label":"flex-layout-integration"}),
    );
    let session = created["session_id"].as_str().unwrap().to_owned();
    let _cleanup = Cleanup(&engine, server.clone(), session.clone());
    rpc(
        &engine,
        &server,
        "session.attach",
        json!({"session_id":session}),
    );
    rpc(
        &engine,
        &server,
        "session.set_label",
        json!({"session_id":session,"label":"flex-renamed"}),
    );
    let window = rpc(
        &engine,
        &server,
        "session.new_window",
        json!({"session_id":session,"layout":{"block":{"flavor":"com.superlogical.terminal.shell"}}}),
    );
    let anchor = window["block_ids"][0].as_str().unwrap().to_owned();
    let second = rpc(
        &engine,
        &server,
        "session.new_split",
        json!({"session_id":session,"anchor_id":anchor,"direction":"horizontal","side":"after","focus":true,"layout":{"block":{"flavor":"com.superlogical.terminal.shell"}}}),
    );
    let block = second["block_ids"][0].as_str().unwrap().to_owned();
    rpc(
        &engine,
        &server,
        "session.float_block",
        json!({"session_id":session,"block_id":block,"bounds":[0.1,0.1,0.5,0.5]}),
    );
    let layer = {
        wait(|| {
            engine
                .state()
                .server(&server)
                .unwrap()
                .sessions
                .iter()
                .flat_map(|s| &s.windows)
                .flat_map(|w| &w.blocks)
                .any(|b| b.id == block && b.floating)
        });
        engine
            .state()
            .server(&server)
            .unwrap()
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.blocks)
            .find(|b| b.id == block)
            .unwrap()
            .layer_id
            .clone()
            .unwrap()
    };
    rpc(
        &engine,
        &server,
        "session.set_layer_state",
        json!({"session_id":session,"layer_id":layer,"state":"minimized"}),
    );
    wait(|| {
        engine
            .state()
            .server(&server)
            .unwrap()
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.blocks)
            .any(|b| b.id == block && b.layer_state == "minimized")
    });
    rpc(
        &engine,
        &server,
        "session.set_layer_state",
        json!({"session_id":session,"layer_id":layer,"state":"normal","focus":true}),
    );
    rpc(
        &engine,
        &server,
        "session.detach_block",
        json!({"session_id":session,"block_id":block}),
    );
    let blocks = rpc(
        &engine,
        &server,
        "session.list_blocks",
        json!({"session_id":session}),
    );
    assert!(blocks["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["block_id"] == block && b["placement"] == "detached"));
    rpc(
        &engine,
        &server,
        "session.move_block",
        json!({"session_id":session,"block_id":block,"anchor_block_id":anchor,"direction":"vertical","side":"after","ratio":0.5}),
    );
    let blocks = rpc(
        &engine,
        &server,
        "session.list_blocks",
        json!({"session_id":session}),
    );
    assert!(blocks["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["block_id"] == block && b["placement"] == "placed"));
    // Tool layers must explicitly take focus; Rex defaults new_layer.focus to false.
    let before_tool = rpc(
        &engine,
        &server,
        "session.view",
        json!({"session_id":session}),
    );
    let window_id = before_tool["active_window_id"].as_str().unwrap();
    let origin = before_tool["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["window_id"] == window_id)
        .unwrap()["focused_block_id"]
        .clone();
    let tool = rpc(
        &engine,
        &server,
        "session.new_layer",
        json!({
            "session_id":session,"window_id":window_id,"bounds":[0.18,0.1,0.64,0.72],"focus":true,
            "layout":{"block":{"flavor":"com.superlogical.terminal.shell"}}
        }),
    );
    let tool_id = tool["block_ids"][0].as_str().unwrap();
    wait(|| {
        engine
            .state()
            .server(&server)
            .unwrap()
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .any(|w| w.id == window_id && w.focused_block_id.as_deref() == Some(tool_id))
    });
    // Like the client completion path, restore the exact origin explicitly:
    // Rex's default layer fallback need not choose that same pane.
    rpc(&engine, &server, "session.focus_block", json!({"session_id":session,"block_id":origin}));
    rpc(
        &engine,
        &server,
        "block.close",
        json!({"session_id":session,"block_id":tool_id}),
    );
    let restored = rpc(
        &engine,
        &server,
        "session.view",
        json!({"session_id":session}),
    );
    assert_eq!(
        restored["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["window_id"] == window_id)
            .unwrap()["focused_block_id"],
        origin
    );
    let creators = rpc(&engine, &server, "block_creator.list", json!({}));
    assert!(creators["creators"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["name"] == "com.superlogical.terminal"));
}
