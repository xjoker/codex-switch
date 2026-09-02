#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn tui_sigterm_restores_its_lifecycle_and_exits_143() {
    let home = tempfile::tempdir().expect("temporary app home");
    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: valid output pointers are supplied; both returned descriptors are
    // immediately owned by `File` values below.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: openpty returned fresh owned descriptors on success.
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let mut child = Command::new(env!("CARGO_BIN_EXE_codex-switch"))
        .arg("tui")
        .env("CODEX_SWITCH_HOME", home.path())
        .env("CODEX_HOME", home.path().join("codex"))
        .stdin(Stdio::from(slave.try_clone().expect("clone PTY")))
        .stdout(Stdio::from(slave.try_clone().expect("clone PTY")))
        .stderr(Stdio::from(slave))
        .spawn()
        .expect("start TUI");

    // SAFETY: `master_fd` remains owned by `master`; fcntl only changes its
    // status flags so readiness can be bounded by a deadline.
    let flags = unsafe { libc::fcntl(master_fd, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    let mut terminal_output = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        match master.read(&mut chunk) {
            Ok(read) if read > 0 => {
                terminal_output.extend_from_slice(&chunk[..read]);
                if terminal_output
                    .windows(b"\x1b[?1049h".len())
                    .any(|window| window == b"\x1b[?1049h")
                {
                    break;
                }
            }
            Ok(0) => panic!("TUI exited before initializing its terminal"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < ready_deadline,
                    "TUI did not initialize its terminal: {:?}",
                    String::from_utf8_lossy(&terminal_output)
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("reading TUI readiness: {error}"),
            Ok(_) => unreachable!(),
        }
    }
    // SAFETY: the PID belongs to the child process spawned immediately above.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("query TUI") {
            break status;
        }
        assert!(Instant::now() < deadline, "TUI did not stop after SIGTERM");
        std::thread::sleep(Duration::from_millis(20));
    };

    assert_eq!(status.code(), Some(143), "status={status:?}");
    assert_eq!(
        status.signal(),
        None,
        "TUI must restore before exiting itself"
    );
}
