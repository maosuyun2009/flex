//! Opt-in tests. FLEX_TEST_SERVER must point at a disposable, isolated Rex server.
//! These tests create no persistent client configuration.
use flex_engine::{Connectivity, Engine};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn wait<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < until, "timed out waiting for Rex");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn call(engine: &Engine, id: &str, method: &str, args: Value) -> Value {
    engine
        .shrew()
        .block_on(engine.call_server(id, method, args))
        .unwrap()
}

#[test]
#[ignore = "requires an isolated Rex server in FLEX_TEST_SERVER"]
fn persistent_identity_scripts_and_same_connection_replies() {
    let address = std::env::var("FLEX_TEST_SERVER").expect("explicit isolated server required");
    let engine = Arc::new(Engine::new(
        None,
        "flex-integration",
        Arc::new(|| {}),
        Arc::new(|_| {}),
    ));
    let server = engine.add_server("isolated test", &address).unwrap();
    wait(|| {
        engine
            .state()
            .server(&server)
            .filter(|s| s.connectivity == Connectivity::Online)
            .map(|_| ())
    });
    call(
        &engine,
        &server,
        "client.set_label",
        json!({"label":"flex-integration-renamed"}),
    );
    let clients = call(&engine, &server, "client.list", json!({}));
    let client = clients["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["info"]["label"] == "flex-integration-renamed")
        .expect("persistent client label")["client_id"]
        .as_str()
        .unwrap()
        .to_owned();
    call(
        &engine,
        &server,
        "client.set_script",
        json!({"scripts":[{"name":"integration.lua","source":"rex.bind('ctrl+x>ctrl+s', function(ctx, ev) return true end)"}]}),
    );
    let map = call(
        &engine,
        &server,
        "client.keymap",
        json!({
            "actions":[{"name":"test.echo","args":{"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}}],
            "server_actions":"all"
        }),
    );
    assert!(map["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["keys"] == json!(["ctrl+x", "ctrl+s"]) && b["action"] == "client.key.forward"));
    let inspect = call(
        &engine,
        &server,
        "client.inspect",
        json!({"client_id":client}),
    );
    assert!(
        inspect.to_string().contains("test.echo"),
        "registered actions belong to persistent client: {inspect}"
    );
    std::thread::scope(|scope| {
        let result = scope.spawn(|| {
            call(
                &engine,
                &server,
                "client.perform",
                json!({
                    "client_id":client,"action":"test.echo","args":{"value":42},"timeout_ms":10000
                }),
            )
        });
        let request = wait(|| engine.take_client_requests().into_iter().next());
        assert_eq!(request.server_id, server);
        assert_eq!(request.request_type, "client_perform_requested");
        assert_eq!(request.payload["args"]["value"], 42);
        engine
            .shrew()
            .block_on(engine.reply_client_request(&request.event_id, Ok(json!({"echo":42}))))
            .unwrap();
        let result = result.join().unwrap();
        assert_eq!(result["result"]["echo"], 42);
        assert_eq!(result["performed_by"], client);
        assert!(
            engine
                .shrew()
                .block_on(engine.reply_client_request(&request.event_id, Ok(Value::Null)))
                .is_err(),
            "reply routes are one-shot"
        );
    });
    let layer_error = engine.shrew().block_on(engine.call_server(&server, "session.set_layer_state", json!({
        "session_id":"missing-test-session", "layer_id":"missing-test-layer", "state":"normal"
    }))).unwrap_err().to_string();
    assert!(
        !layer_error.contains("revision"),
        "new layer operations were not negotiated: {layer_error}"
    );
    call(&engine, &server, "client.set_script", json!({"scripts":[]}));
    let map = call(&engine, &server, "client.keymap", json!({}));
    assert!(!map["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["keys"] == json!(["ctrl+x", "ctrl+s"])));
    engine.shrew().remove_server(&server);
}
