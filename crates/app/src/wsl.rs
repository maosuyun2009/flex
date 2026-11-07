//! The Rex server inside WSL: bring one up on the loopback and hand over its address.
//!
//! Mirrored WSL networking shares the Windows loopback, so a distro listening on
//! `127.0.0.1:7297` answers on the same address here. Nothing needs the distro's
//! own IP, which changes on every NAT-mode boot anyway.

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Where the WSL server listens for this shell. The same string is handed to
/// `rex server start`, so the listener and the address cannot drift apart.
pub(super) const ENDPOINT: &str = "127.0.0.1:7297";

const START: &[&str] = &[
    "-e",
    "rex",
    "server",
    "start",
    "--extra-listen",
    ENDPOINT,
    "--allow-anonymous",
];

/// Whether `address` is the WSL server's, the only one this shell can start.
pub(super) fn is_wsl(address: &str) -> bool {
    address.trim() == ENDPOINT
}

/// Start the WSL server if it is not up, and return the address to connect to.
pub(super) fn connect() -> Result<String, String> {
    let start = |restart: bool| {
        if restart {
            // `--extra-listen` applies only to the instance rex launches, so a
            // server that is already running has to be replaced to gain the port.
            // That ends the programs running in its sessions; ask before calling.
            run(&["-e", "rex", "server", "stop"])?;
        }
        run(START)
    };
    ensure(start, listening).map(|()| ENDPOINT.to_owned())
}

/// Start the server when nothing answers on the endpoint. A server that is
/// already up keeps its sessions: replacing it is what [`connect`] does last.
pub(super) fn start_if_down() -> Result<(), String> {
    start_if_down_with(|| run(START), listening)
}

fn start_if_down_with(
    mut start: impl FnMut() -> Result<(), String>,
    probe: impl Fn() -> bool,
) -> Result<(), String> {
    if probe() {
        return Ok(());
    }
    start()?;
    if probe() {
        return Ok(());
    }
    // Still nothing: the server is up but never took the listener, and only it
    // can give the port up, by stopping.
    Err(format!(
        "no server is answering on {ENDPOINT}; run `rex server stop` inside WSL, then click again"
    ))
}

/// `start(false)` launches the server, `start(true)` replaces a running one;
/// `probe` answers whether the endpoint accepts a connection.
fn ensure(
    mut start: impl FnMut(bool) -> Result<(), String>,
    probe: impl Fn() -> bool,
) -> Result<(), String> {
    start(false)?;
    if probe() {
        return Ok(());
    }
    start(true)?;
    if probe() {
        return Ok(());
    }
    Err(format!(
        "no server is answering on {ENDPOINT}; see `rex server logs` inside WSL"
    ))
}

fn listening() -> bool {
    ENDPOINT
        .parse::<SocketAddr>()
        .ok()
        .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok())
        .is_some()
}

fn run(args: &[&str]) -> Result<(), String> {
    let output = std::process::Command::new("wsl.exe")
        .args(args)
        .output()
        .map_err(|e| format!("wsl.exe: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = text(&output.stderr);
    let stdout = text(&output.stdout);
    let message = if stderr.trim().is_empty() { stdout } else { stderr };
    let message = message.trim();
    Err(if message.is_empty() {
        format!("wsl.exe {args:?} failed: {}", output.status)
    } else {
        message.to_owned()
    })
}

/// wsl.exe's own messages are UTF-16LE; a program's output inside the distro is not.
fn text(bytes: &[u8]) -> String {
    if bytes.contains(&0) {
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&wide)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{ensure, is_wsl, start_if_down_with, text, ENDPOINT};

    #[test]
    fn a_server_that_already_answers_is_left_alone() {
        let mut restarts = 0;
        assert!(ensure(|restart| { restarts += u8::from(restart); Ok(()) }, || true).is_ok());
        assert_eq!(restarts, 0);
    }

    #[test]
    fn a_running_server_without_the_port_is_restarted_once() {
        let restarts = std::cell::Cell::new(0);
        let ok = ensure(
            |restart| {
                if restart {
                    restarts.set(restarts.get() + 1);
                }
                Ok(())
            },
            || restarts.get() > 0,
        );
        assert!(ok.is_ok());
        assert_eq!(restarts.get(), 1);
    }

    #[test]
    fn silence_after_a_restart_names_the_endpoint() {
        let err = ensure(|_| Ok(()), || false).unwrap_err();
        assert!(err.contains(ENDPOINT), "{err}");
    }

    #[test]
    fn a_failing_start_is_reported_once() {
        let mut calls = 0;
        let err = ensure(
            |_| {
                calls += 1;
                Err("no distribution".to_owned())
            },
            || true,
        )
        .unwrap_err();
        assert_eq!(err, "no distribution");
        assert_eq!(calls, 1);
    }

    #[test]
    fn wsl_own_messages_are_read_as_utf16() {
        let bytes: Vec<u8> = "no installed distributions\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(text(&bytes).trim(), "no installed distributions");
        assert_eq!(text(b"rex: no such session\n").trim(), "rex: no such session");
    }

    #[test]
    fn only_the_wsl_endpoint_is_ours() {
        assert!(is_wsl(ENDPOINT));
        assert!(is_wsl(&format!(" {ENDPOINT}\n")));
        assert!(!is_wsl("192.168.1.39:7297"));
        assert!(!is_wsl("localhost:7297"));
        assert!(!is_wsl(""));
    }

    #[test]
    fn a_server_that_answers_is_not_started_again() {
        let mut starts = 0;
        let ok = start_if_down_with(
            || {
                starts += 1;
                Ok(())
            },
            || true,
        );
        assert!(ok.is_ok());
        assert_eq!(starts, 0);
    }

    #[test]
    fn a_missing_server_is_started_once() {
        let starts = std::cell::Cell::new(0);
        let ok = start_if_down_with(
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
            || starts.get() > 0,
        );
        assert!(ok.is_ok());
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn a_server_that_will_not_take_the_port_says_how_to_fix_it() {
        let err = start_if_down_with(|| Ok(()), || false).unwrap_err();
        assert!(err.contains("rex server stop"), "{err}");
    }

    #[test]
    fn a_failing_start_is_reported_by_start_if_down() {
        let err = start_if_down_with(|| Err("no distribution".to_owned()), || false).unwrap_err();
        assert_eq!(err, "no distribution");
    }
}
