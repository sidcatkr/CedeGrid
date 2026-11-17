// Helpers for System mode's container fixtures (005 T005), included by tests/system_mode.rs.
// They run as root inside the system-mode image (tools/docker-validation/system-mode), with
// systemd as PID 1, and act as the test accounts they create through runuser. The accounts and
// their user IDs exist only inside that container.

use std::collections::BTreeMap;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

pub const MEMBER: &str = "member1";
pub const MEMBER_UID: u32 = 2001;
pub const SECOND: &str = "member2";
pub const SECOND_UID: u32 = 2002;
pub const OUTSIDER: &str = "outsider";

/// Whether the fixtures may run: only inside the prepared container.
pub fn enabled() -> bool {
    std::env::var_os("CEDEGRID_TEST_SYSTEM_CONTAINER").is_some()
}

pub fn run(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{program} {args:?}: {error}"))
}

pub fn ok(program: &str, args: &[&str]) -> String {
    let output = run(program, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Runs the installed `cedegrid` as `user`. A reply of `rate_limited`, left by an earlier
/// fixture's burst, is retried after the bucket refills.
pub fn cedegrid_as(user: &str, args: &[&str]) -> Output {
    let mut all = vec!["-u", user, "--", "/usr/bin/cedegrid"];
    all.extend_from_slice(args);
    for _ in 0..20 {
        let output = run("runuser", &all);
        if !String::from_utf8_lossy(&output.stderr).contains("too many requests") {
            return output;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    run("runuser", &all)
}

fn ensure_account(name: &str, uid: Option<u32>) {
    if run("id", &["-u", name]).status.success() {
        return;
    }
    let uid = uid.map(|uid| uid.to_string());
    let mut args = vec!["-m", "-U"];
    if let Some(uid) = &uid {
        args.extend(["-u", uid.as_str()]);
    }
    args.push(name);
    ok("useradd", &args);
}

/// Installs the built binary and the packaging files as the package would, then the accounts.
/// Safe to call more than once.
pub fn install() {
    ensure_account(OUTSIDER, None);
    let binary = std::env::var("CEDEGRID_TEST_BIN").expect("CEDEGRID_TEST_BIN");
    let packaging = std::env::var("CEDEGRID_TEST_PACKAGING").expect("CEDEGRID_TEST_PACKAGING");
    ok("install", &["-o", "root", "-g", "root", "-m", "0755", &binary, "/usr/bin/cedegrid"]);
    for unit in [
        "cedegrid-broker.socket",
        "cedegrid-broker.service",
        "cedegrid.slice",
        "cedegrid_idle.slice",
    ] {
        ok(
            "install",
            &["-m", "0644", &format!("{packaging}/{unit}"), &format!("/usr/lib/systemd/system/{unit}")],
        );
    }
    ok(
        "install",
        &[
            "-m",
            "0644",
            &format!("{packaging}/cedegrid-system.sysusers"),
            "/usr/lib/sysusers.d/cedegrid-system.conf",
        ],
    );
    ok("sh", &[&format!("{packaging}/postinst"), "configure"]);
    ensure_account(MEMBER, Some(MEMBER_UID));
    ensure_account(SECOND, Some(SECOND_UID));
    ensure_account(OUTSIDER, None);
    ok("usermod", &["-aG", "cedegrid", MEMBER]);
    ok("usermod", &["-aG", "cedegrid", SECOND]);
}

/// `systemctl show` for the given properties.
pub fn show(unit: &str, properties: &str) -> BTreeMap<String, String> {
    ok("systemctl", &["show", &format!("--property={properties}"), "--", unit])
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stops a member's units through the client, whatever state they are in.
pub fn stop_units(user: &str) {
    let _ = cedegrid_as(user, &["system", "stop"]);
}

/// Sends raw bytes to the broker as `user` (or as root with `None`) and returns its reply line,
/// `EACCES` when the socket refuses the connection, or `CLOSED` when no reply came.
/// With `hold`, the connection stays open after the bytes are sent, without an end of input.
pub fn raw_request(user: Option<&str>, bytes: &[u8], hold: bool) -> String {
    for _ in 0..20 {
        let reply = raw_request_once(user, bytes, hold);
        if reply_code(&reply) != "rate_limited" {
            return reply;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    raw_request_once(user, bytes, hold)
}

/// `raw_request` without the retry, for the rate limit's own fixture.
pub fn raw_request_once(user: Option<&str>, bytes: &[u8], hold: bool) -> String {
    use std::io::Write;
    const CLIENT: &str = r#"
import socket, sys
data = sys.stdin.buffer.read()
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect("/run/cedegrid/broker.sock")
except PermissionError:
    print("EACCES"); sys.exit(0)
s.settimeout(10)
try:
    s.sendall(data)
except OSError:
    pass
if sys.argv[1] == "close":
    try:
        s.shutdown(socket.SHUT_WR)
    except OSError:
        pass
reply = b""
try:
    while not reply.endswith(b"\n"):
        chunk = s.recv(65536)
        if not chunk:
            break
        reply += chunk
except OSError:
    pass
print(reply.decode().strip() or "CLOSED")
"#;
    let mode = if hold { "hold" } else { "close" };
    let mut command = match user {
        Some(user) => {
            let mut command = Command::new("runuser");
            command.args(["-u", user, "--", "python3", "-c", CLIENT, mode]);
            command
        }
        None => {
            let mut command = Command::new("python3");
            command.args(["-c", CLIENT, mode]);
            command
        }
    };
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("python3");
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The refusal code in a reply line, or `ok`.
pub fn reply_code(reply: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(reply) {
        Ok(value) => value,
        Err(_) => return reply.to_owned(),
    };
    if value["ok"] == true {
        "ok".to_owned()
    } else {
        value["code"].as_str().unwrap_or("?").to_owned()
    }
}

/// Removes what `install` put in place, as `apt purge` would: the removal scripts around the
/// file removal. The test accounts stay.
pub fn purge() {
    let packaging = std::env::var("CEDEGRID_TEST_PACKAGING").expect("CEDEGRID_TEST_PACKAGING");
    let _ = run("sh", &[&format!("{packaging}/prerm"), "purge"]);
    for file in [
        "/usr/lib/systemd/system/cedegrid-broker.socket",
        "/usr/lib/systemd/system/cedegrid-broker.service",
        "/usr/lib/systemd/system/cedegrid.slice",
        "/usr/lib/systemd/system/cedegrid_idle.slice",
        "/usr/lib/sysusers.d/cedegrid-system.conf",
        "/usr/bin/cedegrid",
    ] {
        let _ = std::fs::remove_file(file);
    }
    let _ = run("sh", &[&format!("{packaging}/postrm"), "purge"]);
}

/// What the package may change on the host: unit files and units, the group, its files.
pub fn inventory() -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for line in ok("systemctl", &["list-unit-files", "--no-legend", "--plain"]).lines() {
        if line.contains("cedegrid") {
            items.push(format!("unit-file {}", line.split_whitespace().next().unwrap_or("")));
        }
    }
    for line in ok("systemctl", &["list-units", "--all", "--no-legend", "--plain"]).lines() {
        if line.contains("cedegrid") {
            items.push(format!("unit {}", line.split_whitespace().next().unwrap_or("")));
        }
    }
    if run("getent", &["group", "cedegrid"]).status.success() {
        items.push("group cedegrid".to_owned());
    }
    for path in [
        "/etc/cedegrid",
        "/usr/bin/cedegrid",
        "/run/cedegrid",
        "/usr/lib/sysusers.d/cedegrid-system.conf",
    ] {
        if std::path::Path::new(path).exists() {
            items.push(format!("path {path}"));
        }
    }
    items.sort();
    items
}
