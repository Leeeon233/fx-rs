#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

#[test]
fn tui_starts_an_acp_session_and_quits_from_a_real_pty() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open test PTY");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_fx-tui"));
    command.arg("--acp-exe");
    command.arg(env!("CARGO_BIN_EXE_fx-acp"));
    command.arg("--cwd");
    command.arg(env!("CARGO_MANIFEST_DIR"));
    command.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(command).expect("spawn fx-tui");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = captured.clone();
    let output_reader = std::thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        loop {
            let read = reader.read(&mut chunk).expect("read TUI output");
            if read == 0 {
                break;
            }
            reader_capture
                .lock()
                .unwrap()
                .extend_from_slice(&chunk[..read]);
        }
    });
    let mut writer = pair.master.take_writer().expect("take PTY writer");

    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ready = captured
            .lock()
            .unwrap()
            .windows("Ready".len())
            .any(|window| window == b"Ready");
        if ready {
            break;
        }
        if Instant::now() >= ready_deadline {
            child.kill().expect("kill unready fx-tui");
            let _ = child.wait();
            panic!("fx-tui did not finish ACP session setup");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    writer.write_all(b"/quit\r").expect("send quit command");
    writer.flush().expect("flush quit command");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll fx-tui") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill stalled fx-tui");
            timed_out = true;
            break child.wait().expect("wait for killed fx-tui");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    drop(writer);
    drop(pair.master);
    output_reader.join().expect("join PTY reader");
    let output = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
    assert!(
        !timed_out,
        "fx-tui did not exit after /quit; output:\n{output}"
    );
    assert!(status.success(), "fx-tui exited with {status:?}");
    assert!(output.contains("fxrs"), "TUI did not draw its header");
    assert!(output.contains("Message"), "TUI did not draw its composer");
}
