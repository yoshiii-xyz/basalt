use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use basalt::Database;

struct TempDir {
    path: std::path::PathBuf,
}

static TEMP_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "basalt-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(args)
        .output()
        .unwrap()
}

fn run_with_stdin(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn command_frontend_persists_and_returns_json_lines() {
    let dir = TempDir::new("cli-json");
    let database = dir.path("app.db");
    let path = database.to_str().unwrap();

    let first = run(&[
        "--json",
        "-c",
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO users VALUES (1, 'Ada');",
        path,
    ]);
    assert!(first.status.success(), "{:?}", first);
    let stdout = String::from_utf8(first.stdout).unwrap();
    assert!(stdout.contains("{\"type\":\"create_table\",\"name\":\"users\"}"));
    assert!(stdout.contains("{\"type\":\"insert\",\"rows_affected\":1}"));

    let second = run(&["--json", "-c", "SELECT * FROM users;", path]);
    assert!(second.status.success(), "{:?}", second);
    assert_eq!(
        String::from_utf8(second.stdout).unwrap(),
        "{\"type\":\"select\",\"columns\":[\"id\",\"name\"],\"rows\":[[1,\"Ada\"]]}\n"
    );
}

#[test]
fn script_frontend_supports_csv_and_stdin() {
    let dir = TempDir::new("cli-script");
    let script = dir.path("seed.sql");
    std::fs::write(
        &script,
        "CREATE TABLE notes (id INTEGER, body TEXT); INSERT INTO notes VALUES (1, 'a,b'), (2, 'plain');",
    )
    .unwrap();
    let database = dir.path("script.db");
    let path = database.to_str().unwrap();
    let script_path = script.to_str().unwrap();

    let seeded = run(&["--quiet", "--file", script_path, path]);
    assert!(seeded.status.success(), "{:?}", seeded);
    assert!(seeded.stdout.is_empty());

    let queried = run_with_stdin(
        &["--csv", "--file", "-", path],
        "SELECT * FROM notes ORDER BY id;",
    );
    assert!(queried.status.success(), "{:?}", queried);
    assert_eq!(
        String::from_utf8(queried.stdout).unwrap(),
        "id,body\n1,\"a,b\"\n2,plain\n"
    );
}

#[test]
fn interactive_frontend_handles_meta_commands_and_multiline_sql() {
    let output = run_with_stdin(
        &[":memory:"],
        ".mode json\nCREATE TABLE t (\n  id INTEGER,\n  note TEXT\n);\nINSERT INTO t VALUES (1, 'hello; world');\n.tables\n.schema t\nSELECT * FROM t;\n.quit\n",
    );
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("output mode: json"));
    assert!(stdout.contains("t\n"));
    assert!(stdout.contains("CREATE TABLE \"t\""));
    assert!(stdout.contains("\"rows\":[[1,\"hello; world\"]]"));
}

#[test]
fn invalid_batch_sql_returns_a_failure_status() {
    let output = run(&["--quiet", "-c", "SELEC definitely_not_valid;"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("command line")
    );
}

#[test]
fn durable_database_is_exclusive_across_processes() {
    let dir = TempDir::new("cli-lock");
    let database = dir.path("app.db");
    let path = database.to_str().unwrap();
    let owner = Database::open(&database).unwrap();

    let output = run(&["--quiet", "-c", "SELECT 1", path]);
    assert!(
        !output.status.success(),
        "second process unexpectedly opened the database"
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("already open")
    );

    drop(owner);
}

fn unique_suffix() -> u128 {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed) as u128;
    timestamp * 1_000_000 + sequence
}
