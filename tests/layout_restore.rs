//! Own test binary: the data dir is shared across a restart through a
//! process-wide env var, which would leak into parallel tests elsewhere.

mod common;

use std::thread;
use std::time::Duration;

use common::describe;
use common::grid_contains;
use common::xmpp_fixture::XmppFixture;

const CONTACT: &str = "contact@localhost";
const CLOSED: &str = "closed@localhost";

#[test]
fn restart_restores_open_windows_and_active_window() {
    let data = tempfile::tempdir().unwrap();
    std::env::set_var("APARTE_TEST_DATA_DIR", data.path());

    {
        let first = XmppFixture::new(&[]);
        first.send_command(&format!("/msg {CONTACT}"));
        assert!(first.wait_for(&format!("[{CONTACT}]"), Duration::from_secs(5)));
        first.send_command(&format!("/msg {CLOSED}"));
        assert!(first.wait_for(&format!("[{CLOSED}]"), Duration::from_secs(5)));
        first.send_command(&format!("/close {CLOSED}"));
        first.send_command(&format!("/win {CONTACT}"));
        assert!(first.wait_for(&format!("[{CONTACT}]"), Duration::from_secs(5)));
        thread::sleep(Duration::from_millis(300));
    }

    std::env::set_var("APARTE_TEST_CONNECTED_TEXT", format!("[{CONTACT}]"));
    let second = XmppFixture::new(&[]);
    let parser = second.snapshot();
    assert!(
        !grid_contains(parser.screen(), CLOSED),
        "closed window came back\n{}",
        describe(parser.screen()),
    );
}
