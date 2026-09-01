# Basalt

An embedded SQL database engine built from scratch — no dependencies.

Layers, built in the open:
1. **SQL frontend** — lexer, recursive-descent parser, expression AST ✅
2. **Storage** — catalog, tables, row serialization, pages ⏳
3. **Executor** — scan/filter/project, UPDATE/DELETE ⏳
4. **Indexes** — B-tree, PK/unique constraints ⏳
5. **Transactions** — MVCC, concurrent readers/writers ⏳
6. **Durability** — WAL, crash recovery (kill -9 safe) ⏳
7. **Planner** — JOINs, aggregates, cost-based index choice ⏳
8. **Proof** — torture tests, crash tests, benchmarks vs SQLite ⏳

```sql
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
INSERT INTO users VALUES (1, 'ada');
SELECT * FROM users WHERE id = 1;
```
