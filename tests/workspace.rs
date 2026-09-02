use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "basalt-workspace-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(args)
        .output()
        .expect("Basalt should run")
}

fn path_arg(path: &Path) -> &str {
    path.to_str().expect("test paths should be UTF-8")
}

fn inspect(workspace: &Path) -> Value {
    let output = run(&["workspace", "inspect", "--json", path_arg(workspace)]);
    assert!(output.status.success(), "inspect failed: {output:?}");
    serde_json::from_slice(&output.stdout).expect("inspect should emit JSON")
}

#[test]
fn initializes_and_inspects_a_workspace() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let output = run(&["init", path_arg(&workspace)]);
    assert!(output.status.success(), "init failed: {output:?}");
    assert!(workspace.join("workspace.json").is_file());
    assert!(workspace.join("data.basalt").is_file());

    let report = inspect(&workspace);
    assert_eq!(report["format_version"], 1);
    assert_eq!(report["database"], "data.basalt");
    assert_eq!(report["tables"].as_array().unwrap().len(), 0);
}

#[test]
fn imports_csv_exports_common_formats_and_reopens() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let source = temp.path().join("events.csv");
    let csv = "id,name,note\n1,\"Ada, Lovelace\",\n2,Bob,\"hello\"\n";
    fs::write(&source, csv).unwrap();
    assert!(
        run(&["workspace", "init", path_arg(&workspace)])
            .status
            .success()
    );

    let output = run(&[
        "workspace",
        "import",
        path_arg(&workspace),
        path_arg(&source),
    ]);
    assert!(output.status.success(), "import failed: {output:?}");
    let report = inspect(&workspace);
    assert_eq!(report["tables"][0]["name"], "events");
    assert_eq!(report["tables"][0]["rows"], 2);
    assert_eq!(report["tables"][0]["columns"][0]["data_type"], "INTEGER");

    let query = run(&[
        "workspace",
        "query",
        "--json",
        path_arg(&workspace),
        "SELECT id, name, note FROM events ORDER BY id",
    ]);
    assert!(query.status.success(), "query failed: {query:?}");
    let lines: Vec<Value> = String::from_utf8(query.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["rows"][0][1], "Ada, Lovelace");
    assert_eq!(lines[0]["rows"][0][2], "");

    let rejected = run(&[
        "workspace",
        "query",
        path_arg(&workspace),
        "DELETE FROM events",
    ]);
    assert!(!rejected.status.success());

    let csv_output = temp.path().join("roundtrip.csv");
    let export = run(&[
        "workspace",
        "export",
        path_arg(&workspace),
        "events",
        path_arg(&csv_output),
    ]);
    assert!(export.status.success(), "CSV export failed: {export:?}");
    assert_eq!(
        fs::read_to_string(&csv_output).unwrap(),
        "id,name,note\n1,\"Ada, Lovelace\",\n2,Bob,hello\n"
    );

    let jsonl = run(&[
        "workspace",
        "export",
        "--format",
        "jsonl",
        path_arg(&workspace),
        "events",
        "-",
    ]);
    assert!(jsonl.status.success(), "JSONL export failed: {jsonl:?}");
    let exported: Vec<Value> = String::from_utf8(jsonl.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(exported[0]["id"], 1);
    assert_eq!(exported[0]["name"], "Ada, Lovelace");
    assert_eq!(exported[0]["note"], "");
}

#[test]
fn json_import_and_sql_roundtrip_are_deterministic() {
    let temp = TempDir::new();
    let workspace = temp.path().join("json-workspace");
    let source = temp.path().join("records.json");
    fs::write(
        &source,
        r#"[{"name":"Ada","active":true,"score":3.5},{"name":"Grace","score":9}]"#,
    )
    .unwrap();
    assert!(
        run(&["workspace", "init", path_arg(&workspace)])
            .status
            .success()
    );
    assert!(
        run(&[
            "workspace",
            "import",
            "--table",
            "records",
            path_arg(&workspace),
            path_arg(&source),
        ])
        .status
        .success()
    );

    let dump = temp.path().join("records.sql");
    assert!(
        run(&[
            "workspace",
            "export",
            "--format",
            "sql",
            path_arg(&workspace),
            "records",
            path_arg(&dump),
        ])
        .status
        .success()
    );
    let first_dump = fs::read(&dump).unwrap();
    assert_eq!(first_dump, fs::read(&dump).unwrap());

    let restored = temp.path().join("restored");
    assert!(
        run(&["workspace", "init", path_arg(&restored)])
            .status
            .success()
    );
    let output = run(&["workspace", "import", path_arg(&restored), path_arg(&dump)]);
    assert!(output.status.success(), "SQL import failed: {output:?}");
    let report = inspect(&restored);
    assert_eq!(report["tables"][0]["name"], "records");
    assert_eq!(report["tables"][0]["rows"], 2);
}

#[test]
fn failed_sql_import_does_not_leave_a_partial_table() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let source = temp.path().join("broken.sql");
    fs::write(
        &source,
        "CREATE TABLE broken (id INTEGER); INSERT INTO broken VALUES (1); INSERT INTO broken VALUES ('bad');",
    )
    .unwrap();
    assert!(
        run(&["workspace", "init", path_arg(&workspace)])
            .status
            .success()
    );
    let output = run(&[
        "workspace",
        "import",
        path_arg(&workspace),
        path_arg(&source),
    ]);
    assert!(!output.status.success());
    assert!(inspect(&workspace)["tables"].as_array().unwrap().is_empty());
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
