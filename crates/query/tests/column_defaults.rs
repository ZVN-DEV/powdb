use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::types::Value;

/// A literal column default fills an omitted column on insert, and the default
/// survives a restart (it is persisted in the catalog, not just held in memory).
#[test]
fn powql_default_fills_omitted_column_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut engine = Engine::new(dir.path()).unwrap();
        engine
            .execute_powql(
                r#"type T { required id: int, status: str default "active", qty: int default 0 }"#,
            )
            .unwrap();
        // Omit status + qty — both should be filled from their defaults.
        engine.execute_powql("insert T { id := 1 }").unwrap();
    }

    // Reopen the database: defaults must be reloaded from the catalog so a
    // later insert still applies them.
    let mut engine = Engine::new(dir.path()).unwrap();
    engine.execute_powql("insert T { id := 2 }").unwrap();

    match engine
        .execute_powql("T order .id { .id, .status, .qty }")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(
                rows[0],
                vec![Value::Int(1), Value::Str("active".into()), Value::Int(0)],
                "row inserted before restart uses defaults"
            );
            assert_eq!(
                rows[1],
                vec![Value::Int(2), Value::Str("active".into()), Value::Int(0)],
                "row inserted after restart uses reloaded defaults"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// SQL `CREATE TABLE ... DEFAULT <literal>` lowers to the same default
/// mechanism, so an omitted column is filled on `INSERT`.
#[test]
fn sql_create_table_default_fills_omitted_column() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_sql(
            "CREATE TABLE T (id INTEGER NOT NULL UNIQUE, status TEXT DEFAULT 'active', qty INTEGER DEFAULT 0)",
        )
        .unwrap();
    engine.execute_sql("INSERT INTO T (id) VALUES (1)").unwrap();
    match engine.execute_sql("SELECT id, status, qty FROM T").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0],
                vec![Value::Int(1), Value::Str("active".into()), Value::Int(0)]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// A `required` column that declares a default may be omitted on insert — the
/// default is applied before the required-column check.
#[test]
fn required_column_with_default_can_be_omitted() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql(r#"type T { required id: int, required status: str default "new" }"#)
        .unwrap();
    engine.execute_powql("insert T { id := 1 }").unwrap();
    match engine.execute_powql("T { .status }").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("new".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// A default whose literal type does not match the column type is rejected at
/// table-create time, not silently coerced or deferred to insert.
#[test]
fn default_of_wrong_type_is_rejected_at_create() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    let err = engine
        .execute_powql(r#"type T { id: int default "not an int" }"#)
        .unwrap_err();
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("type") || msg.contains("coerce") || msg.contains("expected"),
        "wrong-type default should be a type error, got: {err}"
    );
}

/// An `upsert` that inserts a new row (no conflict) applies column defaults
/// for omitted columns, same as a plain insert.
#[test]
fn upsert_insert_applies_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql(r#"type T { required unique id: int, status: str default "active" }"#)
        .unwrap();
    engine.execute_powql("upsert T on .id { id := 1 }").unwrap();
    match engine.execute_powql("T { .status }").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("active".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// An explicit value provided for a column overrides its default.
#[test]
fn explicit_value_overrides_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::new(dir.path()).unwrap();
    engine
        .execute_powql(r#"type T { required id: int, status: str default "active" }"#)
        .unwrap();
    engine
        .execute_powql(r#"insert T { id := 1, status := "banned" }"#)
        .unwrap();
    match engine.execute_powql("T { .status }").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0][0], Value::Str("banned".into()));
        }
        other => panic!("expected rows, got {other:?}"),
    }
}
