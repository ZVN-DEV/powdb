use powdb_oracle::compare::diff;
use powdb_oracle::engines::powdb::Powdb;
use powdb_oracle::engines::sqlite::Sqlite;
use powdb_oracle::fixture;
use powdb_oracle::model::{ColType, Kind};

fn main() {
    let fixtures = fixture::all();
    for name in ["boundary", "boundary_indexed"] {
        let fx = fixtures.iter().find(|f| f.name == name).expect("fx");
        let mut pdb = Powdb::open(fx).expect("powdb");
        let sq = Sqlite::open(fx).expect("sqlite");
        println!("== {name}");
        for (col, ty) in [("i", ColType::Int), ("f", ColType::Float)] {
            for limit in [1usize, 2, 3, 7, 12, 40] {
                for desc in [true, false] {
                    let dir = if desc { "desc" } else { "asc" };
                    let sdir = if desc { "DESC" } else { "ASC" };
                    let fast = pdb.powql(&format!(
                        "T order .{col} {dir} limit {limit} {{ .id, .{col} }}"
                    ));
                    let generic = pdb.powql(&format!(
                        "T order .{col} {dir} limit {limit} offset 0 {{ .id, .{col} }}"
                    ));
                    let reference = sq.query(
                        &format!("SELECT id, {col} FROM T ORDER BY {col} IS NULL ASC, {col} {sdir}, id ASC LIMIT {limit}"),
                        &[], &[Kind::Col(ColType::Int), Kind::Col(ty)]);
                    let a = diff(&fast, &generic, true);
                    let b = diff(&fast, &reference, true);
                    let c = diff(&generic, &reference, true);
                    if a.is_some() || b.is_some() || c.is_some() {
                        println!("  {col} limit={limit} {dir}: fast-vs-generic={a:?} fast-vs-sqlite={b:?} generic-vs-sqlite={c:?}");
                    }
                }
            }
        }
        println!("  (nothing printed above => all agreed)");
        println!(
            "  EXPLAIN no-offset: {:?}",
            pdb.powql("explain T order .i desc limit 12 { .id, .i }")
        );
    }
}
