//! Opt-in file roundtrip against an explicitly disposable Rex server.
use flex_engine::files::{FileTarget, TransferProgress};
use flex_engine::{Connectivity, Engine};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn rpc(engine: &Engine, id: &str, method: &str, args: Value) -> Value {
    engine
        .shrew()
        .block_on(engine.call_server(id, method, args))
        .unwrap()
}
struct Cleanup<'a> {
    engine: &'a Engine,
    server: String,
    session: String,
    local: PathBuf,
}
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = self.engine.shrew().block_on(self.engine.call_server(
            &self.server,
            "session.destroy",
            json!({"session_id":self.session}),
        ));
        let _ = std::fs::remove_dir_all(&self.local);
    }
}
#[test]
#[ignore = "requires disposable FLEX_FILE_TEST_SERVER; creates temporary files and a session"]
fn native_batch_upload_and_atomic_download_roundtrip() {
    let address = std::env::var("FLEX_FILE_TEST_SERVER").expect("disposable test server required");
    let engine = Engine::new(
        None,
        "file-transfer-test",
        Arc::new(|| {}),
        Arc::new(|_| {}),
    );
    let server = engine.add_server("isolated files", &address).unwrap();
    let until = Instant::now() + Duration::from_secs(12);
    while !engine
        .state()
        .server(&server)
        .is_some_and(|s| s.connectivity == Connectivity::Online)
    {
        assert!(Instant::now() < until, "server not ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    let session = rpc(
        &engine,
        &server,
        "session.create",
        json!({"label":"files-roundtrip","initial_windows":[{"layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{"shell":"none","command":["cat"]}}}}]}),
    );
    let sid = session["session_id"].as_str().unwrap().to_owned();
    let bid = session["initial_windows"][0]["block_ids"][0]
        .as_str()
        .unwrap()
        .to_owned();
    rpc(
        &engine,
        &server,
        "session.attach",
        json!({"session_id":sid}),
    );
    let local = std::env::temp_dir().join(format!("flex-file-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&local).unwrap();
    let _cleanup = Cleanup {
        engine: &engine,
        server: server.clone(),
        session: sid.clone(),
        local: local.clone(),
    };
    let source = local.join("roundtrip.bin");
    let empty = local.join("empty.bin");
    let expected: Vec<_> = (0..180_000).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &expected).unwrap();
    std::fs::write(&empty, []).unwrap();
    let target = FileTarget {
        server_id: server.clone(),
        address: address.clone(),
        session_id: sid.clone(),
        block_id: bid.clone(),
    };
    let progress: Arc<dyn Fn(TransferProgress) + Send + Sync> =
        Arc::new(|p| println!("progress: {} {}", p.file_name, p.completed_bytes));
    engine
        .shrew()
        .block_on(engine.upload_files(
            target.clone(),
            vec![source.clone(), empty.clone()],
            Arc::new(AtomicBool::new(false)),
            progress.clone(),
        ))
        .expect("native batch upload");
    let text = rpc(
        &engine,
        &server,
        "com.superlogical.terminal.format",
        json!({"session_id":sid,"block_id":bid,"args":{"format":"text","trim":true,"unwrap":true}}),
    )["content"]
        .as_str()
        .unwrap()
        .to_owned();
    println!("pasted paths: {text:?}");
    let paths: Vec<_> = text
        .split_whitespace()
        .map(|s| s.trim_matches(['\'', '"']))
        .filter(|s| s.ends_with(".bin"))
        .collect();
    assert_eq!(
        paths.len(),
        2,
        "one batch must paste exactly two paths in order"
    );
    assert!(paths[0].ends_with("roundtrip.bin"));
    assert!(paths[1].ends_with("empty.bin"));
    let dest = local.join("download.bin");
    engine
        .shrew()
        .block_on(engine.download_file(
            target.clone(),
            paths[0].into(),
            dest.clone(),
            false,
            Arc::new(AtomicBool::new(false)),
            progress.clone(),
        ))
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    let empty_dest = local.join("empty-downloaded.bin");
    engine
        .shrew()
        .block_on(engine.download_file(
            target.clone(),
            paths[1].into(),
            empty_dest.clone(),
            false,
            Arc::new(AtomicBool::new(false)),
            progress.clone(),
        ))
        .unwrap();
    assert_eq!(std::fs::metadata(&empty_dest).unwrap().len(), 0);
    std::fs::write(&dest, b"keep-existing").unwrap();
    assert!(engine
        .shrew()
        .block_on(engine.download_file(
            target.clone(),
            paths[0].into(),
            dest.clone(),
            false,
            Arc::new(AtomicBool::new(false)),
            progress.clone()
        ))
        .is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"keep-existing");
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop = cancelled.clone();
    let stop_after_chunk: Arc<dyn Fn(TransferProgress) + Send + Sync> = Arc::new(move |_| {
        stop.store(true, Ordering::Release);
    });
    assert!(engine
        .shrew()
        .block_on(engine.download_file(
            target.clone(),
            paths[0].into(),
            dest.clone(),
            true,
            cancelled,
            stop_after_chunk
        ))
        .is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"keep-existing");
    assert!(
        std::fs::read_dir(&local).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp")),
        "cancel leaked a temporary file"
    );
    engine
        .shrew()
        .block_on(engine.download_file(
            target.clone(),
            paths[0].into(),
            dest.clone(),
            true,
            Arc::new(AtomicBool::new(false)),
            progress.clone(),
        ))
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    assert!(
        engine
            .shrew()
            .block_on(engine.upload_files(
                target.clone(),
                vec![source, local.clone()],
                Arc::new(AtomicBool::new(false)),
                progress.clone()
            ))
            .is_err(),
        "directory must reject whole batch"
    );
    let invalid = FileTarget {
        address: "127.0.0.1:1".into(),
        ..target
    };
    assert!(engine
        .shrew()
        .block_on(engine.download_file(
            invalid,
            paths[0].into(),
            local.join("wrong.bin"),
            false,
            Arc::new(AtomicBool::new(false)),
            progress
        ))
        .is_err());
}
