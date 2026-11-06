//! End-to-end against a real Rex server. Skipped unless SHREW_TEST_SERVER is set:
//!   SHREW_TEST_SERVER=nixos-server cargo test -p shrew-core --test live -- --nocapture

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use shrew_core::*;

struct L;
impl Listener for L {
    fn state_changed(&self) {}
    fn notify(&self, n: Notification) {
        eprintln!("notify: {n:?}");
    }
}

#[derive(Default)]
struct T {
    frames: AtomicUsize,
}
impl TerminalListener for T {
    fn frame_ready(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }
    fn status_changed(&self, s: TerminalStatus) {
        eprintln!("terminal: {s:?}");
    }
    fn clipboard(&self, _: String) {}
    fn size_changed(&self, c: u16, r: u16) {
        eprintln!("size {c}x{r}");
    }
}

fn wait<F: FnMut() -> bool>(secs: u64, mut f: F) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn screen(t: &TerminalSession) -> String {
    t.frame().lines.iter().map(|r| r.runs.iter().map(|x| x.text.as_str()).collect::<String>().trim_end().to_owned() + "\n").collect()
}

#[test]
fn full_round_trip() {
    let Ok(addr) = std::env::var("SHREW_TEST_SERVER") else {
        eprintln!("SHREW_TEST_SERVER not set; skipping");
        return;
    };
    let shrew = Shrew::new(Config::default(), Arc::new(L));
    let t0 = Instant::now();
    let id = shrew.add_server("", &addr).unwrap();
    assert!(wait(10, || matches!(shrew.state().server(&id).unwrap().connectivity, Connectivity::Online)), "{:?}", shrew.state());
    eprintln!("online in {:?}", t0.elapsed());

    let label = format!("shrew-it-{}", std::process::id());
    let (sid, bid) = shrew.block_on(shrew.create_session(&id, Some(&label), None)).unwrap();
    let bid = bid.unwrap();
    // Destroy the session even if an assertion below fails.
    struct Cleanup<'a>(&'a Shrew, String, String);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.block_on(self.0.destroy_session(&self.1, &self.2));
        }
    }
    let _cleanup = Cleanup(&shrew, id.clone(), sid.clone());
    assert!(wait(5, || shrew.state().server(&id).unwrap().sessions.iter().any(|s| s.id == sid && s.block_count() == 1)));

    let tl = Arc::new(T::default());
    let term = shrew.open_terminal(&id, &sid, &bid, tl.clone()).unwrap();
    assert!(wait(10, || term.status() == TerminalStatus::Live), "never went live");
    let t1 = Instant::now();
    term.send_text("printf 'shrew-%s\\n' 42");
    term.send_key(Key::Enter, false, false, false, false);
    assert!(wait(10, || screen(&term).contains("shrew-42\n")), "output never showed:\n{}", screen(&term));
    eprintln!("echo round trip {:?}, {} frames", t1.elapsed(), tl.frames.load(Ordering::Relaxed));

    // Split, then make sure the model sees two blocks.
    shrew.block_on(shrew.split(&id, &sid, &bid, false, None)).unwrap();
    assert!(wait(5, || shrew.state().server(&id).unwrap().sessions.iter().any(|s| s.id == sid && s.block_count() == 2)));

    // Ctrl+C and a password-style prompt don't break the input queue.
    term.send_text("sleep 30");
    term.send_key(Key::Enter, false, false, false, false);
    term.send_key(Key::Char('c'), true, false, false, false);
    term.send_text("echo after-ctrl-c\r");
    assert!(wait(10, || screen(&term).contains("after-ctrl-c\n")), "ctrl-c failed:\n{}", screen(&term));

    // Palette: the server's titled actions, and running one on this block.
    let actions = shrew.block_on(shrew.palette_actions(&id)).unwrap();
    eprintln!("palette: {:?}", actions.iter().map(|a| &a.title).collect::<Vec<_>>());
    let clear = actions.iter().find(|a| a.name == "com.superlogical.terminal.clear").expect("clear action");
    shrew.block_on(shrew.run_action(&id, &sid, Some(&bid), &clear.name)).unwrap();
    assert!(wait(5, || !screen(&term).contains("after-ctrl-c")), "clear didn't clear:\n{}", screen(&term));

    // A request with no usable live connection (dropped, or dead after the app
    // was frozen in the background) goes over a short-lived connection instead.
    shrew.set_foreground(false);
    shrew.disconnect_for_test(&id);
    shrew.block_on(shrew.send_text(&id, &sid, &bid, "echo bg-reply\r")).expect("reply without a live connection");
    shrew.set_foreground(true);
    assert!(wait(15, || screen(&term).contains("bg-reply")), "background reply missing:\n{}", screen(&term));

    term.close();
    drop(_cleanup);
    assert!(wait(5, || !shrew.state().server(&id).unwrap().sessions.iter().any(|s| s.id == sid)));
}
