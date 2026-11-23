//! Regression test for GitHub issue #1:
//! `open --session-id <id>` must keep Chromium alive after the CLI exits
//! (even on Linux where `PR_SET_PDEATHSIG` is used for ephemeral runs).

use std::process::Command;

struct SessionGuard {
    bin: &'static str,
    id: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = Command::new(self.bin)
            .args(["close", "--session-id", &self.id])
            .output();
    }
}

#[test]
fn persistent_session_survives_cli_exit_and_accepts_act() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("skipping persistent_session test: no Chrome binary found");
        return;
    }

    let bin = env!("CARGO_BIN_EXE_flashbrowse");
    let session_id = format!("test-persist-{}", std::process::id());
    let _guard = SessionGuard {
        bin,
        id: session_id.clone(),
    };

    let url = "data:text/html,<button onclick=\"document.body.append('clicked')\">Go</button>";
    let open = Command::new(bin)
        .args(["open", url, "--session-id", &session_id])
        .output()
        .expect("run flashbrowse open");
    assert!(
        open.status.success(),
        "open failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&open.stdout),
        String::from_utf8_lossy(&open.stderr)
    );
    let open_out = String::from_utf8_lossy(&open.stdout);
    assert!(open_out.contains("[1] Button: \"Go\""), "{open_out}");

    // The `open` CLI process has now exited; `act` must still reach Chromium.
    let act = Command::new(bin)
        .args(["act", "1", "--session-id", &session_id, "--action", "click"])
        .output()
        .expect("run flashbrowse act");
    assert!(
        act.status.success(),
        "act failed after open exited: stdout={}, stderr={}",
        String::from_utf8_lossy(&act.stdout),
        String::from_utf8_lossy(&act.stderr)
    );
    let act_out = String::from_utf8_lossy(&act.stdout);
    assert!(act_out.contains("clicked"), "{act_out}");

    let close = Command::new(bin)
        .args(["close", "--session-id", &session_id])
        .output()
        .expect("run flashbrowse close");
    assert!(
        close.status.success(),
        "close failed: stderr={}",
        String::from_utf8_lossy(&close.stderr)
    );
}
