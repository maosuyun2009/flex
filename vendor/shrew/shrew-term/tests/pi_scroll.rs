//! Recorded synthetic pi-tui redraws, not a user's conversation or a live model.
use shrew_term::{Frame, Terminal};

fn text(frame: &Frame) -> String {
    frame.lines.iter().map(|row| {
        row.runs.iter().map(|run| run.text.as_str()).collect::<String>().trim_end().to_owned()
    }).collect::<Vec<_>>().join("\n")
}

#[test]
fn pi_scroll_and_concurrent_output_publish_only_complete_frames() {
    let mut terminal = Terminal::new(60, 12);
    terminal.write_vt(include_bytes!("fixtures/pi-scroll/initial.vt"));
    assert_eq!(text(&terminal.frame()), include_str!("fixtures/pi-scroll/initial.txt"));
    for (bytes, expected) in [
        (&include_bytes!("fixtures/pi-scroll/scroll-up.vt")[..], include_str!("fixtures/pi-scroll/scroll-up.txt")),
        (&include_bytes!("fixtures/pi-scroll/output.vt")[..], include_str!("fixtures/pi-scroll/output.txt")),
        (&include_bytes!("fixtures/pi-scroll/bottom.vt")[..], include_str!("fixtures/pi-scroll/bottom.txt")),
    ] {
        let before = terminal.frame();
        // Deliberately split a pi render into many transport packets and render
        // between each pair. No intermediate screen may replace the last frame.
        for (index, chunk) in bytes.chunks(17).enumerate() {
            terminal.write_vt(chunk);
            let current = terminal.frame();
            if (index + 1) * 17 < bytes.len() {
                assert_eq!(current, before, "painted a half-finished pi frame at packet {index}");
            } else {
                assert_eq!(text(&current), expected);
            }
        }
    }
    assert!(text(&terminal.frame()).contains("NEW OUTPUT WHILE SCROLLED UP"));
}
