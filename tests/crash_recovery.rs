use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use basalt::{Database, db::StatementResult};

static TEMP_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn abrupt_process_exit_recovers_the_wal_commit() {
    let dir = std::env::temp_dir().join(format!(
        "basalt-crash-test-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("main.db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(["--crash-test-writer", path.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    let ready = child
        .stdout
        .take()
        .map(|stdout| BufReader::new(stdout).read_line(&mut line).is_ok())
        .unwrap_or(false);
    if !ready || line.trim() != "ready" {
        let _ = child.kill();
        let _ = child.wait();
        panic!("crash-test writer did not become ready");
    }
    let _ = child.kill();
    let status = child.wait().unwrap();
    assert!(!status.success());

    let database = Database::open(&path).unwrap();
    let result = database.execute_sql("SELECT * FROM crash_probe").unwrap();
    assert!(matches!(
        &result[0],
        StatementResult::Select { rows, .. }
            if rows.len() == 1 && rows[0][1] == basalt::types::Value::Text("durable".into())
    ));
    let _ = std::fs::remove_dir_all(dir);
}

fn unique_suffix() -> u128 {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed) as u128;
    timestamp * 1_000_000 + sequence
}
