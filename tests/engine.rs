//! L2/L3 integration tests: full SQL sessions through parser + executor.

use basalt::Database;
use basalt::db::{State, StatementResult};
use basalt::engine;
use basalt::sql::parser::parse;
use basalt::types::Value;

fn run(db: &mut State, sql: &str) -> StatementResult {
    let stmts = parse(sql).unwrap_or_else(|e| panic!("parse error: {} for {sql:?}", e.message));
    let mut last = None;
    for s in stmts {
        last =
            Some(engine::execute(db, &s).unwrap_or_else(|e| panic!("exec error: {e} for {sql:?}")));
    }
    last.expect("at least one statement")
}

fn run_err(db: &mut State, sql: &str) -> String {
    let stmts = parse(sql).unwrap();
    let mut msg = None;
    for s in stmts {
        if let Err(e) = engine::execute(db, &s) {
            msg = Some(e.message);
        }
    }
    msg.expect("expected an error")
}

fn selected(db: &mut State, sql: &str) -> (Vec<String>, Vec<Vec<String>>) {
    match run(db, sql) {
        StatementResult::Select { columns, rows } => {
            let s: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(cell).collect()).collect();
            (columns, s)
        }
        other => panic!("not a select: {other:?}"),
    }
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => format!("{f}"),
        Value::Text(s) => s.clone(),
        Value::Boolean(b) => b.to_string(),
    }
}

#[test]
fn create_insert_select_roundtrip() {
    let mut db = State::empty();
    run(
        &mut db,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL)",
    );
    run(
        &mut db,
        "INSERT INTO users VALUES (1, 'ada', 3.5), (2, 'grace', 9.1)",
    );
    let (cols, rows) = selected(&mut db, "SELECT * FROM users");
    assert_eq!(cols, vec!["id", "name", "score"]);
    assert_eq!(
        rows,
        vec![vec!["1", "ada", "3.5"], vec!["2", "grace", "9.1"]]
    );
}

#[test]
fn insert_column_subset_fills_null() {
    let mut db = State::empty();
    run(&mut db, "CREATE TABLE t (a INTEGER, b TEXT, c REAL)");
    run(&mut db, "INSERT INTO t (a, c) VALUES (1, 2.5)");
    let (_, rows) = selected(&mut db, "SELECT * FROM t");
    assert_eq!(rows, vec![vec!["1", "NULL", "2.5"]]);
}

#[test]
fn insert_no_column_list_uses_table_order() {
    let mut db = State::empty();
    run(&mut db, "CREATE TABLE t (x INTEGER, y TEXT)");
    run(&mut db, "INSERT INTO t VALUES (7, 'seven')");
    let (_, rows) = selected(&mut db, "SELECT * FROM t");
    assert_eq!(rows, vec![vec!["7", "seven"]]);
}

#[test]
fn unknown_table_error() {
    let mut db = State::empty();
    assert!(run_err(&mut db, "SELECT * FROM nope").contains("no such table"));
    assert!(run_err(&mut db, "INSERT INTO nope VALUES (1)").contains("no such table"));
}

#[test]
fn duplicate_column_name_rejected() {
    let mut db = State::empty();
    assert!(run_err(&mut db, "CREATE TABLE t (a INTEGER, a INTEGER)").contains("duplicate column"));
}

#[test]
fn type_coercion_on_insert() {
    let mut db = State::empty();
    run(&mut db, "CREATE TABLE t (i INTEGER, r REAL, b BOOLEAN)");
    run(&mut db, "INSERT INTO t VALUES (1.0 + 0, 1 + 1, 1)");
    let (_, rows) = selected(&mut db, "SELECT * FROM t");
    assert_eq!(rows, vec![vec!["1", "2", "true"]]);
    assert!(run_err(&mut db, "INSERT INTO t VALUES ('abc', 1, 1)").contains("cannot convert"));
}

#[test]
fn numeric_literals_support_exponents_and_minimum_integer() {
    let mut db = State::empty();
    let (_, rows) = selected(&mut db, "SELECT .5, 1e3, 1.25E-2, -9223372036854775808");
    assert_eq!(
        rows,
        vec![vec!["0.5", "1000", "0.0125", "-9223372036854775808"]]
    );
}

#[test]
fn select_where_projection_order_limit() {
    let mut db = State::empty();
    run(
        &mut db,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL)",
    );
    run(
        &mut db,
        "INSERT INTO users VALUES (1, 'ada', 3.5), (2, 'grace', 9.1), (3, 'linus', 7.0), (4, 'ken', 2.1)",
    );
    let (cols, rows) = selected(
        &mut db,
        "SELECT name FROM users WHERE score > 3 ORDER BY score DESC LIMIT 2",
    );
    assert_eq!(cols, vec!["name"]);
    assert_eq!(rows, vec![vec!["grace"], vec!["linus"]]);
    // expression projection: name, score*2 ordered by score asc
    let (_, rows) = selected(
        &mut db,
        "SELECT name, score * 2 FROM users WHERE score > 3 ORDER BY score",
    );
    assert_eq!(
        rows,
        vec![vec!["ada", "7"], vec!["linus", "14"], vec!["grace", "18.2"]]
    );
}

#[test]
fn distinct_and_empty_table() {
    let mut db = State::empty();
    run(&mut db, "CREATE TABLE t (a INTEGER, b INTEGER)");
    run(&mut db, "INSERT INTO t VALUES (1, 2), (1, 2), (3, 4)");
    let (_, rows) = selected(&mut db, "SELECT DISTINCT a, b FROM t");
    assert_eq!(rows, vec![vec!["1", "2"], vec!["3", "4"]]);
    run(&mut db, "CREATE TABLE empty (x INTEGER)");
    let (_, rows) = selected(&mut db, "SELECT * FROM empty");
    assert!(rows.is_empty());
    assert!(run_err(&mut db, "SELECT missing FROM empty").contains("no such column"));
    assert!(run_err(&mut db, "DELETE FROM empty WHERE missing = 1").contains("no such column"));
}

#[test]
fn null_sorts_lowest() {
    let mut db = State::empty();
    run(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, NULL), (2, 5), (3, 1)");
    let (_, rows) = selected(&mut db, "SELECT id FROM t ORDER BY v");
    assert_eq!(rows, vec![vec!["1"], vec!["3"], vec!["2"]]);
    let (_, rows) = selected(&mut db, "SELECT id FROM t ORDER BY v DESC");
    assert_eq!(rows, vec![vec!["2"], vec!["3"], vec!["1"]]);
}

#[test]
fn update_and_delete_where() {
    let mut db = State::empty();
    run(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)");
    run(&mut db, "UPDATE t SET v = v + 1 WHERE id = 2");
    let (_, rows) = selected(&mut db, "SELECT v FROM t ORDER BY id");
    assert_eq!(rows, vec![vec!["10"], vec!["21"], vec!["30"]]);
    run(&mut db, "DELETE FROM t WHERE v >= 30");
    let (_, rows) = selected(&mut db, "SELECT id FROM t");
    assert_eq!(rows, vec![vec!["1"], vec!["2"]]);
    run(&mut db, "DELETE FROM t");
    let (_, rows) = selected(&mut db, "SELECT * FROM t");
    assert!(rows.is_empty());
}

#[test]
fn update_multi_assignment_part1() {
    let mut db = State::empty();
    run(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, 1, 1)");
    run(&mut db, "UPDATE t SET a = a + 10, b = a + 20 WHERE id = 1");
    let (_, rows) = selected(&mut db, "SELECT a, b FROM t");
    assert_eq!(rows, vec![vec!["11", "21"]]);
}

#[test]
fn three_valued_where_excludes_unknown() {
    let mut db = State::empty();
    run(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, NULL), (2, 5)");
    let (_, rows) = selected(&mut db, "SELECT id FROM t WHERE v = v");
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn not_null_pk_unique_violations() {
    let mut db = State::empty();
    run(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE, nn INTEGER NOT NULL)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, 'a', 5)");
    assert!(run_err(&mut db, "INSERT INTO t VALUES (1, 'b', 6)").contains("already exists"));
    assert!(run_err(&mut db, "INSERT INTO t VALUES (2, 'a', 6)").contains("already exists"));
    assert!(run_err(&mut db, "INSERT INTO t VALUES (2, 'b', NULL)").contains("NOT NULL"));
}

#[test]
fn joins_grouping_and_scalar_functions() {
    let mut db = State::empty();
    run(
        &mut db,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
    );
    run(
        &mut db,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
    );
    run(&mut db, "INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace')");
    run(
        &mut db,
        "INSERT INTO posts VALUES (10, 1, 'First'), (11, 1, 'Second')",
    );
    let (columns, rows) = selected(
        &mut db,
        "SELECT u.name, COUNT(p.id) FROM users AS u LEFT JOIN posts AS p ON u.id = p.user_id GROUP BY u.name ORDER BY u.name",
    );
    assert_eq!(columns, vec!["u.name", "COUNT"]);
    assert_eq!(rows, vec![vec!["Ada", "2"], vec!["Grace", "0"]]);
    let (_, rows) = selected(
        &mut db,
        "SELECT UPPER(name), LENGTH(name) FROM users ORDER BY id",
    );
    assert_eq!(rows, vec![vec!["ADA", "3"], vec!["GRACE", "5"]]);
    let (columns, rows) = selected(&mut db, "SELECT u.* FROM users u ORDER BY u.id");
    assert_eq!(columns, vec!["id", "name"]);
    assert_eq!(rows, vec![vec!["1", "Ada"], vec!["2", "Grace"]]);
    let (_, rows) = selected(&mut db, "SELECT name AS n FROM users ORDER BY n DESC");
    assert_eq!(rows, vec![vec!["Grace"], vec!["Ada"]]);
    let (_, rows) = selected(&mut db, "SELECT COUNT(*) FROM users");
    assert_eq!(rows, vec![vec!["2"]]);
    let (_, rows) = selected(
        &mut db,
        "SELECT name AS n, COUNT(*) AS c FROM users GROUP BY name ORDER BY c DESC",
    );
    assert_eq!(rows, vec![vec!["Ada", "1"], vec!["Grace", "1"]]);
    let (_, rows) = selected(
        &mut db,
        "SELECT u.id, p.id FROM users u RIGHT JOIN posts p ON u.id = p.user_id ORDER BY p.id",
    );
    assert_eq!(rows, vec![vec!["1", "10"], vec!["1", "11"]]);
    let (_, rows) = selected(
        &mut db,
        "SELECT u.id, p.id FROM users u FULL JOIN posts p ON u.id = p.user_id ORDER BY p.id",
    );
    assert_eq!(rows.len(), 3);
    let (_, rows) = selected(&mut db, "SELECT 1 + 2, COUNT(*)");
    assert_eq!(rows, vec![vec!["3", "1"]]);
}

#[test]
fn custom_index_and_update_are_atomic() {
    let mut db = State::empty();
    run(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, value INTEGER UNIQUE)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20)");
    run(&mut db, "CREATE INDEX value_idx ON t(value)");
    let error = run_err(&mut db, "UPDATE t SET value = 10");
    assert!(error.contains("already exists"));
    let (_, rows) = selected(&mut db, "SELECT value FROM t ORDER BY id");
    assert_eq!(rows, vec![vec!["10"], vec!["20"]]);
    let error = run_err(&mut db, "INSERT INTO t VALUES (3, 20), (4, 40)");
    assert!(error.contains("already exists"));
    let (_, rows) = selected(&mut db, "SELECT id FROM t ORDER BY id");
    assert_eq!(rows, vec![vec!["1"], vec!["2"]]);
}

#[test]
fn duplicate_target_columns_are_rejected_atomically() {
    let mut db = State::empty();
    run(&mut db, "CREATE TABLE t (a INTEGER, b INTEGER)");
    run(&mut db, "INSERT INTO t VALUES (1, 2)");
    assert!(run_err(&mut db, "INSERT INTO t (a, b, a) VALUES (3, 4, 5)").contains("duplicate"));
    assert!(run_err(&mut db, "UPDATE t SET a = 3, b = 4, a = 5").contains("duplicate"));
    let (_, rows) = selected(&mut db, "SELECT * FROM t");
    assert_eq!(rows, vec![vec!["1", "2"]]);
}

#[test]
fn connection_transactions_commit_and_rollback() {
    let db = Database::in_memory();
    db.execute_sql("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .unwrap();
    let mut connection = db.connect();
    connection
        .execute_sql("BEGIN; INSERT INTO t VALUES (1); ROLLBACK")
        .unwrap();
    let result = db.execute_sql("SELECT * FROM t").unwrap();
    assert!(matches!(&result[0], StatementResult::Select { rows, .. } if rows.is_empty()));
    connection
        .execute_sql("BEGIN; INSERT INTO t VALUES (2); COMMIT")
        .unwrap();
    let result = db.execute_sql("SELECT * FROM t").unwrap();
    assert!(matches!(&result[0], StatementResult::Select { rows, .. } if rows.len() == 1));
}

#[test]
fn insert_select_explain_and_checkpoint() {
    let db = Database::in_memory();
    db.execute_sql(
        "CREATE TABLE source (id INTEGER PRIMARY KEY, value INTEGER); CREATE TABLE copy (id INTEGER, value INTEGER);",
    )
    .unwrap();
    db.execute_sql("INSERT INTO source VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    db.execute_sql("CREATE INDEX source_value ON source(value)")
        .unwrap();
    db.execute_sql("INSERT INTO copy SELECT id, value FROM source WHERE value >= 20")
        .unwrap();
    let results = db.execute_sql("SELECT * FROM copy ORDER BY id").unwrap();
    let StatementResult::Select { rows, .. } = &results[0] else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    let results = db
        .execute_sql("EXPLAIN SELECT * FROM source WHERE id = 2")
        .unwrap();
    assert!(matches!(&results[0], StatementResult::Explain(text) if text.contains("Scan")));
    assert!(matches!(
        db.execute_sql("CHECKPOINT").unwrap().as_slice(),
        [StatementResult::Checkpoint]
    ));
}
