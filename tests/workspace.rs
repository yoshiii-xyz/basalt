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

fn run_with_env(args: &[&str], key: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_basalt"))
        .args(args)
        .env(key, "1")
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
fn machine_readable_import_and_export_reports_are_unambiguous() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let source = temp.path().join("events.csv");
    let export_path = temp.path().join("events.jsonl");
    let source_bytes = b"id,name\n1,Ada\n";
    fs::write(&source, source_bytes).unwrap();
    assert!(run(&["init", path_arg(&workspace)]).status.success());

    let imported = run(&[
        "workspace",
        "import",
        "--json",
        "--table",
        "events",
        path_arg(&workspace),
        path_arg(&source),
    ]);
    assert!(imported.status.success(), "import failed: {imported:?}");
    let imported: Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(imported["operation"], "import");
    assert_eq!(imported["format"], "csv");
    assert_eq!(imported["table"], "events");
    assert_eq!(imported["bytes"], source_bytes.len());
    assert_eq!(imported["summary"], "table events (1 rows, 2 columns)");

    let exported = run(&[
        "workspace",
        "export",
        "--json",
        "--format",
        "jsonl",
        path_arg(&workspace),
        "events",
        path_arg(&export_path),
    ]);
    assert!(exported.status.success(), "export failed: {exported:?}");
    let exported: Value = serde_json::from_slice(&exported.stdout).unwrap();
    assert_eq!(exported["operation"], "export");
    assert_eq!(exported["format"], "jsonl");
    assert_eq!(exported["table"], "events");
    assert_eq!(exported["rows"], 1);
    assert_eq!(exported["bytes"], fs::metadata(&export_path).unwrap().len());

    let ambiguous = run(&[
        "workspace",
        "export",
        "--json",
        "--format",
        "jsonl",
        path_arg(&workspace),
        "events",
        "-",
    ]);
    assert!(!ambiguous.status.success());
    assert!(
        String::from_utf8_lossy(&ambiguous.stderr)
            .contains("--json cannot be combined with stdout export")
    );
}

#[test]
fn refuses_export_paths_that_alias_workspace_metadata() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let source = temp.path().join("events.csv");
    fs::write(&source, "id,name\n1,Ada\n").unwrap();
    assert!(run(&["init", path_arg(&workspace)]).status.success());
    assert!(
        run(&[
            "workspace",
            "import",
            "--table",
            "events",
            path_arg(&workspace),
            path_arg(&source),
        ])
        .status
        .success()
    );

    for protected_file in ["data.basalt", "workspace.json"] {
        let alias = workspace.join("..").join("workspace").join(protected_file);
        let output = run(&[
            "workspace",
            "export",
            "--format",
            "csv",
            path_arg(&workspace),
            "events",
            path_arg(&alias),
        ]);
        assert!(
            !output.status.success(),
            "export unexpectedly succeeded: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("refusing to overwrite workspace metadata or database"),
            "unexpected export error: {output:?}"
        );
    }

    assert_eq!(inspect(&workspace)["tables"][0]["rows"], 1);
}

#[test]
fn rejects_preview_with_too_many_mutating_statements() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    assert!(run(&["init", path_arg(&workspace)]).status.success());
    let sql = (0..33)
        .map(|index| format!("CREATE TABLE table_{index} (id INTEGER)"))
        .collect::<Vec<_>>()
        .join("; ");

    let output = run(&["workspace", "preview", path_arg(&workspace), sql.as_str()]);
    assert!(
        !output.status.success(),
        "preview unexpectedly succeeded: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("preview accepts at most 32 mutating statements"),
        "unexpected preview error: {output:?}"
    );
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

#[test]
fn previews_applies_diffs_and_undoes_one_change() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let source = temp.path().join("users.csv");
    fs::write(&source, "id,name\n1,Ada\n").unwrap();
    assert!(run(&["init", path_arg(&workspace)]).status.success());
    assert!(
        run(&[
            "workspace",
            "import",
            "--table",
            "users",
            path_arg(&workspace),
            path_arg(&source),
        ])
        .status
        .success()
    );

    let preview = run(&[
        "workspace",
        "preview",
        "--json",
        path_arg(&workspace),
        "UPDATE users SET name = 'Grace' WHERE id = 1",
    ]);
    assert!(preview.status.success(), "preview failed: {preview:?}");
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["mutating_statements"], 1);
    assert_eq!(preview["statements"][0]["rows_affected"], 1);
    let plan_id = preview["plan_id"].as_str().unwrap();
    assert_eq!(
        preview["sql"],
        "UPDATE users SET name = 'Grace' WHERE id = 1"
    );

    let apply = run(&[
        "workspace",
        "apply",
        "--json",
        path_arg(&workspace),
        plan_id,
    ]);
    assert!(apply.status.success(), "apply failed: {apply:?}");
    let apply: Value = serde_json::from_slice(&apply.stdout).unwrap();
    let change_id = apply["change_id"].as_str().unwrap();

    let retried_apply = run(&[
        "workspace",
        "apply",
        "--json",
        path_arg(&workspace),
        plan_id,
    ]);
    assert!(
        retried_apply.status.success(),
        "retrying apply failed: {retried_apply:?}"
    );
    let retried_apply: Value = serde_json::from_slice(&retried_apply.stdout).unwrap();
    assert_eq!(retried_apply["change_id"], change_id);

    let query = run(&[
        "workspace",
        "query",
        "--json",
        path_arg(&workspace),
        "SELECT name FROM users",
    ]);
    let query: Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(query["rows"][0][0], "Grace");

    let diff = run(&[
        "workspace",
        "diff",
        "--json",
        path_arg(&workspace),
        change_id,
    ]);
    assert!(diff.status.success(), "diff failed: {diff:?}");
    let diff: Value = serde_json::from_slice(&diff.stdout).unwrap();
    assert_eq!(diff["precision"], "table-level logical comparison");
    assert_eq!(diff["tables"][0]["data_changed"], true);

    let undo = run(&[
        "workspace",
        "undo",
        "--json",
        path_arg(&workspace),
        change_id,
    ]);
    assert!(undo.status.success(), "undo failed: {undo:?}");
    let undo: Value = serde_json::from_slice(&undo.stdout).unwrap();
    assert_eq!(undo["undone_change_id"], change_id);

    let retried_undo = run(&[
        "workspace",
        "undo",
        "--json",
        path_arg(&workspace),
        change_id,
    ]);
    assert!(
        retried_undo.status.success(),
        "retrying undo failed: {retried_undo:?}"
    );
    let retried_undo: Value = serde_json::from_slice(&retried_undo.stdout).unwrap();
    assert_eq!(retried_undo["undone_change_id"], change_id);

    let query = run(&[
        "workspace",
        "query",
        "--json",
        path_arg(&workspace),
        "SELECT name FROM users",
    ]);
    let query: Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(query["rows"][0][0], "Ada");
}

#[test]
fn stale_plans_and_non_latest_undo_are_rejected() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let source = temp.path().join("users.csv");
    fs::write(&source, "id,name\n1,Ada\n").unwrap();
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
            "users",
            path_arg(&workspace),
            path_arg(&source),
        ])
        .status
        .success()
    );
    let first = run(&[
        "workspace",
        "preview",
        "--json",
        path_arg(&workspace),
        "UPDATE users SET name = 'Grace' WHERE id = 1",
    ]);
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    let first_plan = first["plan_id"].as_str().unwrap();
    let second = run(&[
        "workspace",
        "preview",
        "--json",
        path_arg(&workspace),
        "UPDATE users SET name = 'Linus' WHERE id = 1",
    ]);
    let second: Value = serde_json::from_slice(&second.stdout).unwrap();
    let second_plan = second["plan_id"].as_str().unwrap();
    let second_apply = run(&[
        "workspace",
        "apply",
        "--json",
        path_arg(&workspace),
        second_plan,
    ]);
    assert!(second_apply.status.success());
    let second_apply: Value = serde_json::from_slice(&second_apply.stdout).unwrap();
    let second_change = second_apply["change_id"].as_str().unwrap();

    let stale = run(&["workspace", "apply", path_arg(&workspace), first_plan]);
    assert!(!stale.status.success());
    let third = run(&[
        "workspace",
        "preview",
        "--json",
        path_arg(&workspace),
        "UPDATE users SET name = 'Alan' WHERE id = 1",
    ]);
    let third: Value = serde_json::from_slice(&third.stdout).unwrap();
    let third_plan = third["plan_id"].as_str().unwrap();
    let third_apply = run(&[
        "workspace",
        "apply",
        "--json",
        path_arg(&workspace),
        third_plan,
    ]);
    assert!(third_apply.status.success());
    let third_apply: Value = serde_json::from_slice(&third_apply.stdout).unwrap();
    let third_change = third_apply["change_id"].as_str().unwrap();
    let old_undo = run(&["workspace", "undo", path_arg(&workspace), second_change]);
    assert!(!old_undo.status.success());
    let latest_undo = run(&[
        "workspace",
        "undo",
        "--json",
        path_arg(&workspace),
        third_change,
    ]);
    assert!(latest_undo.status.success(), "undo failed: {latest_undo:?}");

    let moved_preview = run(&[
        "workspace",
        "preview",
        "--json",
        path_arg(&workspace),
        "UPDATE users SET name = 'Ada' WHERE id = 1",
    ]);
    let moved_preview: Value = serde_json::from_slice(&moved_preview.stdout).unwrap();
    let moved_plan = moved_preview["plan_id"].as_str().unwrap();
    let moved_apply = run(&[
        "workspace",
        "apply",
        "--json",
        path_arg(&workspace),
        moved_plan,
    ]);
    assert!(moved_apply.status.success());

    let replay = run(&["workspace", "apply", path_arg(&workspace), second_plan]);
    assert!(!replay.status.success());
    assert!(
        String::from_utf8_lossy(&replay.stderr).contains("will not be replayed"),
        "unexpected replay error: {replay:?}"
    );
}

#[test]
fn interrupted_apply_and_undo_are_reconciled_after_restart() {
    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let source = temp.path().join("users.csv");
    fs::write(&source, "id,name\n1,Ada\n").unwrap();
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
            "users",
            path_arg(&workspace),
            path_arg(&source),
        ])
        .status
        .success()
    );
    let preview = run(&[
        "workspace",
        "preview",
        "--json",
        path_arg(&workspace),
        "UPDATE users SET name = 'Grace' WHERE id = 1",
    ]);
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    let plan_id = preview["plan_id"].as_str().unwrap();

    let crashed_apply = run_with_env(
        &[
            "--crash-test-workspace-apply",
            path_arg(&workspace),
            plan_id,
        ],
        "BASALT_CRASH_TEST_AFTER_APPLY_CHECKPOINT",
    );
    assert!(!crashed_apply.status.success());
    let history = run(&["workspace", "history", "--json", path_arg(&workspace)]);
    assert!(history.status.success(), "history failed: {history:?}");
    let history: Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(history[0]["status"], "recovered");
    let change_id = history[0]["change_id"].as_str().unwrap();

    let crashed_undo = run_with_env(
        &[
            "--crash-test-workspace-undo",
            path_arg(&workspace),
            change_id,
        ],
        "BASALT_CRASH_TEST_AFTER_UNDO_RESTORE",
    );
    assert!(!crashed_undo.status.success());
    let history = run(&["workspace", "history", "--json", path_arg(&workspace)]);
    let history: Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(history[1]["kind"], "undo");
    assert_eq!(history[1]["status"], "recovered");
    let query = run(&[
        "workspace",
        "query",
        "--json",
        path_arg(&workspace),
        "SELECT name FROM users",
    ]);
    let query: Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(query["rows"][0][0], "Ada");
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_history_directory() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    assert!(run(&["init", path_arg(&workspace)]).status.success());
    symlink(&outside, workspace.join("history")).unwrap();

    let output = run(&["workspace", "history", path_arg(&workspace)]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("symbolic link"),
        "unexpected error: {output:?}"
    );
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
