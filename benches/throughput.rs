use std::time::Instant;

use basalt::{Database, db::StatementResult};

fn main() {
    let database = Database::in_memory();
    database
        .execute_sql("CREATE TABLE measurements (id INTEGER PRIMARY KEY, value REAL)")
        .unwrap();
    let start = Instant::now();
    for id in 0..10_000 {
        database
            .execute_sql(&format!(
                "INSERT INTO measurements VALUES ({id}, {value})",
                value = id as f64 / 10.0
            ))
            .unwrap();
    }
    let insert_elapsed = start.elapsed();
    let start = Instant::now();
    let result = database
        .execute_sql("SELECT COUNT(*), SUM(value) FROM measurements WHERE id >= 5000")
        .unwrap();
    let query_elapsed = start.elapsed();
    let rows = match &result[0] {
        StatementResult::Select { rows, .. } => rows.len(),
        _ => 0,
    };
    println!(
        "Basalt: inserted 10000 rows in {:?}; aggregate query ({rows} row) in {:?}",
        insert_elapsed, query_elapsed
    );
}
