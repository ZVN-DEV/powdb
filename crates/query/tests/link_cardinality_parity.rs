//! RULE L: a link's cardinality is **derived from index uniqueness at the
//! moment of the read**. There is no stored answer.
//!
//! `docs/POWQL.md` has always promised "cardinality is derived, not declared",
//! but the catalog derived it once in `create_link` and stored the answer, so
//! `alter <Target> add unique .<key>` after the link existed left every surface
//! reporting the old cardinality forever: the scalar hop was refused with a
//! message asserting the target key "is not unique" at a moment when it
//! demonstrably was, and `describe <Target>` printed `unique` and `to-many` in
//! the same output. Two databases with a byte-identical final schema behaved in
//! opposite, mutually exclusive ways depending only on DDL order.
//!
//! The fix deletes the stored answer rather than repairing it.
//! [`powdb_storage::catalog::LinkDef::kind`] survives only because the v7
//! on-disk layout has a byte in that position; it is written once at declare
//! time, never refreshed, and **deliberately goes stale**. Every engine
//! decision calls `Catalog::derive_link_kind` / `Catalog::link_kind` instead.
//! Keeping the byte stale is what makes this file able to fail: if any site
//! regresses to reading it, the `link`-then-`unique` ordering below observes
//! the opposite cardinality and the parity assertion fires.
//!
//! **The enumeration.** Six sites in the engine answer "what is this link's
//! cardinality?", and every one of them is listed in [`SITES`] with a query
//! that observes it:
//!
//! | site | code | observed by |
//! |------|------|-------------|
//! | B1 | `dispatch.rs::resolve_via_link` | block traversal legality |
//! | B2 | `dispatch.rs::resolve_scalar_link_field` (per hop) | scalar path legality |
//! | B3 | `scan.rs::introspect_describe`, outgoing loop | `describe <Owner>` |
//! | B4 | `scan.rs::introspect_describe`, incoming loop | `describe <Target>` |
//! | B5 | `scan.rs::introspect_list_links` | `schema links` |
//! | B6 | `lowering.rs::explain_*_link_cardinality` | `explain` labels |
//!
//! B3 and B4 are two separate loops over the same registry, so the schema
//! carries a type that is *only* an owner (`Invoice`) and types that are *only*
//! targets (`Company`, `Item`): reverting one loop fails a probe that names it,
//! rather than hiding behind the other. `no_engine_source_reads_the_advisory_byte`
//! closes the enumeration from the other end, failing on a seventh site added
//! anywhere in engine source, including code paths no probe reaches.
//!
//! The other axes, and where each is enforced:
//!
//! - **order**: unique-then-link, link-then-unique, interleaved, inline-unique
//!   (`ddl_script`), plus plain-index-then-link and link-then-plain-index
//!   (`plain_index_orderings_agree_and_stay_to_many`).
//! - **lifetime**: same process, after reopen, after a hard crash with pending
//!   WAL records, and with a catalog byte that has been rewritten on disk to
//!   the wrong value.
//! - **cache**: every probe runs twice in one process and both runs must match
//!   (`observe`), because resolution happens per execution and a fix that only
//!   ran on a plan-cache miss would pass every other leg.
//!
//! On B6: the three `explain_*` probes this file carried in round 1 were
//! decoration, and were called out as such. `lowering.rs` labelled a block
//! "to-many link" and a path "scalar to-one path" from the query's SYNTAX, so
//! those strings were byte-identical under every ordering and under every
//! mutation of the other sites: they could not fail, and `explain` mislabelled
//! a block written over a to-one link. The cross-type lane fixed that in the
//! same round by routing both labels through `Catalog::link_kind`, which turned
//! `explain` into a genuine sixth site of RULE L. The probes below are the
//! replacement: they now have the schema axis the old ones lacked, and they
//! assert `lowering.rs`'s behaviour without this lane editing its file.

use powdb_query::executor::Engine;
use powdb_query::result::QueryResult;
use powdb_storage::catalog::{Catalog, LinkDef, LinkKind};
use powdb_storage::stored_json_path::{StoredJsonPathSegmentV1, StoredJsonPathV1};
use powdb_storage::types::{ColumnDef, Schema, TypeId, Value};

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "powdb_link_cardinality_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

// ---------------------------------------------------------------------------
// The matrix: one final schema, four DDL orderings
// ---------------------------------------------------------------------------

/// The DDL orderings that must all reach the same behaviour. `LinkThenUnique`
/// is the one that was permanently broken, and it is also the ordering that
/// leaves the advisory byte disagreeing with the truth, which is what gives
/// every assertion below something to catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    UniqueThenLink,
    LinkThenUnique,
    Interleaved,
    InlineUnique,
}

const ORDERS: [Order; 4] = [
    Order::UniqueThenLink,
    Order::LinkThenUnique,
    Order::Interleaved,
    Order::InlineUnique,
];

const TYPES_PLAIN: &[&str] = &[
    "type Company { required id: int, required name: str }",
    "type User { required id: int, required name: str, company_id: int }",
    "type Order { required id: int, user_id: int, required total: float }",
    "type Item { required id: int, order_id: int, required sku: str }",
    "type Invoice { required id: int, user_id: int }",
];

const TYPES_INLINE_UNIQUE: &[&str] = &[
    "type Company { required unique id: int, required name: str }",
    "type User { required unique id: int, required name: str, company_id: int }",
    "type Order { required id: int, user_id: int, required total: float }",
    "type Item { required id: int, order_id: int, required sku: str }",
    "type Invoice { required id: int, user_id: int }",
];

const UNIQUES: &[&str] = &["alter Company add unique .id", "alter User add unique .id"];

/// Three to-one links (unique target key), two to-many links (non-unique target
/// key), one self-link whose owner and target are the same type. `Invoice` owns
/// a link and is the target of none, which isolates the outgoing `describe`
/// loop; `Company` and `Item` are targets and own none, which isolates the
/// incoming one.
const LINKS: &[&str] = &[
    "link Order.user -> User on user_id = id",
    "link User.company -> Company on company_id = id",
    "link User.orders -> Order on id = user_id",
    "link User.same -> User on id = id",
    "link Order.items -> Item on id = order_id",
    "link Invoice.user -> User on user_id = id",
];

const DATA: &[&str] = &[
    r#"insert Company { id := 10, name := "acme" }"#,
    r#"insert User { id := 1, name := "alice", company_id := 10 }"#,
    r#"insert User { id := 2, name := "bob" }"#,
    "insert Order { id := 1, user_id := 1, total := 9.5 }",
    "insert Order { id := 2, user_id := 1, total := 20.25 }",
    "insert Order { id := 3, user_id := 2, total := 5.5 }",
    r#"insert Item { id := 1, order_id := 1, sku := "a" }"#,
    "insert Invoice { id := 1, user_id := 1 }",
];

fn ddl_script(order: Order) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |stmts: &[&str]| out.extend(stmts.iter().map(|s| (*s).to_string()));
    match order {
        Order::UniqueThenLink => {
            push(TYPES_PLAIN);
            push(UNIQUES);
            push(LINKS);
        }
        Order::LinkThenUnique => {
            push(TYPES_PLAIN);
            push(LINKS);
            push(UNIQUES);
        }
        Order::Interleaved => {
            push(TYPES_PLAIN);
            push(&[
                "link Order.user -> User on user_id = id",
                "alter User add unique .id",
                "link User.company -> Company on company_id = id",
                "alter Company add unique .id",
                "link User.orders -> Order on id = user_id",
                "link User.same -> User on id = id",
                "link Order.items -> Item on id = order_id",
                "link Invoice.user -> User on user_id = id",
            ]);
        }
        Order::InlineUnique => {
            push(TYPES_INLINE_UNIQUE);
            push(LINKS);
        }
    }
    push(DATA);
    out
}

/// One row per enumerated site observation. `site` is the code site being
/// exercised, `label` names the observation so a mismatch is attributable, and
/// every entry must also have a pinned expectation in [`assert_pinned`] or the
/// pin walk panics.
struct Site {
    site: &'static str,
    label: &'static str,
    query: &'static str,
}

const SITES: &[Site] = &[
    // B1: block traversal legality. The first entry is the one that catches a
    // revert: with the stale byte saying to-many, the block would be *allowed*
    // through a to-one link and return rows instead of the pinned refusal.
    Site {
        site: "B1",
        label: "block_through_to_one",
        query: "Order as o { o.id, u: o.user { .name } }",
    },
    Site {
        site: "B1",
        label: "block_through_to_many",
        query: "User as u { u.name, orders: u.orders { total } }",
    },
    Site {
        site: "B1",
        label: "block_filtered",
        query: "User as u { u.name, orders: u.orders filter total > 6.0 order total desc limit 1 { total } }",
    },
    Site {
        site: "B1",
        label: "block_on_second_owner",
        query: "Order as o { o.id, items: o.items { sku } }",
    },
    // B2: scalar path legality, one check per hop.
    Site {
        site: "B2",
        label: "scalar_path",
        query: "Order as o { o.id, o.user.name }",
    },
    Site {
        site: "B2",
        label: "multi_hop_scalar",
        query: "Order as o { o.id, o.user.company.name }",
    },
    Site {
        site: "B2",
        label: "self_link_scalar",
        query: "User as u { u.id, u.same.name }",
    },
    Site {
        site: "B2",
        label: "scalar_through_to_many",
        query: "User as u { u.name, u.orders.total }",
    },
    Site {
        site: "B2",
        label: "scalar_and_block_together",
        query: "User as u { u.name, u.company.name, orders: u.orders { total } }",
    },
    // B3: the outgoing `describe` loop. `Invoice` is the target of no link, so
    // this observation can only come from the outgoing loop.
    Site {
        site: "B3",
        label: "describe_owner_only_to_one",
        query: "describe Invoice",
    },
    Site {
        site: "B3",
        label: "describe_outgoing_to_many",
        query: "describe Order",
    },
    // B4: the incoming loop. `Company` and `Item` own no links, so these two
    // observations can only come from the incoming loop.
    Site {
        site: "B4",
        label: "describe_target_only_to_one",
        query: "describe Company",
    },
    Site {
        site: "B4",
        label: "describe_target_only_to_many",
        query: "describe Item",
    },
    // B5: `schema links`.
    Site {
        site: "B5",
        label: "schema_links",
        query: "schema links",
    },
    // B6: EXPLAIN's cardinality words. These were decoration until the
    // cross-type lane made them schema-derived this round (`lowering.rs`
    // `explain_link_cardinality` / `explain_scalar_link_cardinality`); they now
    // call `Catalog::link_kind`, so they are a real site of RULE L and are
    // enumerated here. Probed only, never edited: `lowering.rs` is not this
    // lane's file.
    Site {
        site: "B6",
        label: "explain_scalar_to_one",
        query: "explain Order as o { o.id, o.user.name }",
    },
    Site {
        site: "B6",
        label: "explain_scalar_multi_hop",
        query: "explain Order as o { o.id, o.user.company.name }",
    },
    Site {
        site: "B6",
        label: "explain_scalar_to_many",
        query: "explain User as u { u.name, u.orders.total }",
    },
    Site {
        site: "B6",
        label: "explain_block_to_many",
        query: "explain User as u { u.name, orders: u.orders { total } }",
    },
    Site {
        site: "B6",
        label: "explain_block_over_to_one",
        query: "explain Order as o { o.id, u: o.user { .name } }",
    },
];

fn outcome(engine: &mut Engine, query: &str) -> String {
    match engine.execute_powql(query) {
        Ok(result) => format!("ok {result:?}"),
        Err(err) => format!("err {err}"),
    }
}

/// Run every enumerated observation twice in one process. Resolution happens
/// per execution on a cloned plan, so a fix that only ran on a plan-cache miss
/// would look correct on the first run and stale on the second; requiring both
/// runs to match makes that failure mode visible here rather than in
/// production.
fn observe(engine: &mut Engine) -> Vec<(String, String)> {
    SITES
        .iter()
        .map(|s| {
            let first = outcome(engine, s.query);
            let second = outcome(engine, s.query);
            assert_eq!(
                first, second,
                "site {} probe `{}` changed between the first and second execution of \
                 byte-identical text in one process (plan-cache leg)",
                s.site, s.label
            );
            (format!("{}/{}", s.site, s.label), second)
        })
        .collect()
}

fn build(order: Order, dir: &std::path::Path) -> Engine {
    let mut engine = Engine::new(dir).unwrap();
    for stmt in ddl_script(order) {
        engine
            .execute_powql(&stmt)
            .unwrap_or_else(|e| panic!("{order:?}: `{stmt}` failed: {e}"));
    }
    engine
}

fn rows_of(engine: &mut Engine, query: &str) -> Vec<Vec<Value>> {
    match engine.execute_powql(query) {
        Ok(QueryResult::Rows { rows, .. }) => rows,
        other => panic!("expected rows from `{query}`, got {other:?}"),
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::Str(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// `schema links` reduced to (owner, name, cardinality).
fn link_cardinalities(engine: &mut Engine) -> Vec<(String, String, String)> {
    rows_of(engine, "schema links")
        .iter()
        .map(|r| (text_of(&r[0]), text_of(&r[1]), text_of(&r[5])))
        .collect()
}

/// The `index` column of a `describe` row, keyed by the `column` column.
fn describe_index_column(engine: &mut Engine, ty: &str) -> Vec<(String, String)> {
    rows_of(engine, &format!("describe {ty}"))
        .iter()
        .map(|r| (text_of(&r[0]), text_of(&r[3])))
        .collect()
}

fn expected_cardinalities() -> Vec<(String, String, String)> {
    [
        ("Invoice", "user", "to-one"),
        ("Order", "items", "to-many"),
        ("Order", "user", "to-one"),
        ("User", "company", "to-one"),
        ("User", "orders", "to-many"),
        ("User", "same", "to-one"),
    ]
    .iter()
    .map(|(o, n, c)| ((*o).to_string(), (*n).to_string(), (*c).to_string()))
    .collect()
}

/// EXPLAIN renders as text; assert on the exact cardinality phrase rather than
/// on the whole plan dump, which carries unrelated node detail.
fn assert_explain_contains(engine: &mut Engine, ctx: &str, query: &str, want: &str) {
    let text = rows_of(engine, query)
        .iter()
        .map(|r| text_of(&r[0]))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains(want),
        "{ctx}: EXPLAIN must name the cardinality the catalog gives right now.\nwant: {want}\ngot:\n{text}"
    );
}

fn describe_row(engine: &mut Engine, ty: &str, column: &str) -> String {
    let rows = describe_index_column(engine, ty);
    rows.iter()
        .find(|(c, _)| c == column)
        .map(|(_, detail)| detail.clone())
        .unwrap_or_else(|| panic!("`describe {ty}` has no row for `{column}`: {rows:?}"))
}

// ---------------------------------------------------------------------------
// The pinned answers, one per enumerated observation
// ---------------------------------------------------------------------------

/// Parity alone passes if every ordering is uniformly wrong, so each enumerated
/// observation also has the answer the *current schema* implies pinned here.
/// The `match` is exhaustive by panic: a site added to [`SITES`] without a pin
/// fails the walk instead of silently riding along as parity-only.
fn assert_pinned(engine: &mut Engine, ctx: &str, label: &str) {
    match label {
        "block_through_to_one" => {
            let err = engine
                .execute_powql("Order as o { o.id, u: o.user { .name } }")
                .expect_err(&format!(
                    "{ctx}: a block over a to-one link must be refused"
                ))
                .to_string();
            assert!(
                err.contains(
                    "link `user` on type `Order` is a to-one link (its target key \
                     `User.id` is unique, so a hop matches at most one row); traverse \
                     it as a path (`o.user.<column>`), not a block"
                ),
                "{ctx}: {err}"
            );
        }
        "block_through_to_many" => {
            let rows = rows_of(engine, "User as u { u.name, orders: u.orders { total } }");
            assert_eq!(rows.len(), 2, "{ctx}: one row per user");
        }
        "block_filtered" => {
            let rows = rows_of(
                engine,
                "User as u { u.name, orders: u.orders filter total > 6.0 order total desc limit 1 { total } }",
            );
            assert_eq!(rows.len(), 2, "{ctx}: one row per user");
        }
        "block_on_second_owner" => {
            let rows = rows_of(engine, "Order as o { o.id, items: o.items { sku } }");
            assert_eq!(rows.len(), 3, "{ctx}: one row per order");
        }
        "scalar_path" => {
            let rows = rows_of(engine, "Order as o { o.id, o.user.name }");
            let names: Vec<String> = rows.iter().map(|r| text_of(&r[1])).collect();
            assert_eq!(names, vec!["alice", "alice", "bob"], "{ctx}: scalar hop");
        }
        "multi_hop_scalar" => {
            let rows = rows_of(engine, "Order as o { o.id, o.user.company.name }");
            assert_eq!(text_of(&rows[0][1]), "acme", "{ctx}: multi-hop");
        }
        "self_link_scalar" => {
            let rows = rows_of(engine, "User as u { u.id, u.same.name }");
            assert_eq!(text_of(&rows[0][1]), "alice", "{ctx}: self link");
        }
        "scalar_through_to_many" => {
            let err = engine
                .execute_powql("User as u { u.name, u.orders.total }")
                .expect_err(&format!(
                    "{ctx}: a scalar hop over a to-many link is refused"
                ))
                .to_string();
            assert!(
                err.contains("link `orders` on type `User` is a to-many link"),
                "{ctx}: {err}"
            );
        }
        "scalar_and_block_together" => {
            let rows = rows_of(
                engine,
                "User as u { u.name, u.company.name, orders: u.orders { total } }",
            );
            assert_eq!(text_of(&rows[0][1]), "acme", "{ctx}: scalar beside a block");
        }
        "describe_owner_only_to_one" => {
            assert_eq!(
                describe_row(engine, "Invoice", "user"),
                "-> User (to-one, user_id -> id)",
                "{ctx}: outgoing describe loop, on a type nothing targets"
            );
        }
        "describe_outgoing_to_many" => {
            assert_eq!(
                describe_row(engine, "Order", "items"),
                "-> Item (to-many, id -> order_id)",
                "{ctx}: outgoing describe loop, to-many direction"
            );
            assert_eq!(
                describe_row(engine, "Order", "user"),
                "-> User (to-one, user_id -> id)",
                "{ctx}: outgoing describe loop, to-one direction"
            );
        }
        "describe_target_only_to_one" => {
            assert_eq!(
                describe_row(engine, "Company", "User.company"),
                "<- User (to-one, company_id -> id)",
                "{ctx}: incoming describe loop, on a type that owns no link"
            );
        }
        "describe_target_only_to_many" => {
            assert_eq!(
                describe_row(engine, "Item", "Order.items"),
                "<- Order (to-many, id -> order_id)",
                "{ctx}: incoming describe loop, to-many direction"
            );
        }
        "schema_links" => {
            assert_eq!(
                link_cardinalities(engine),
                expected_cardinalities(),
                "{ctx}: `schema links` cardinality column"
            );
        }
        "explain_scalar_to_one" => {
            assert_explain_contains(
                engine,
                ctx,
                "explain Order as o { o.id, o.user.name }",
                "scalar to-one path o.user.name",
            );
        }
        "explain_scalar_multi_hop" => {
            assert_explain_contains(
                engine,
                ctx,
                "explain Order as o { o.id, o.user.company.name }",
                "scalar to-one path o.user.company.name",
            );
        }
        "explain_scalar_to_many" => {
            assert_explain_contains(
                engine,
                ctx,
                "explain User as u { u.name, u.orders.total }",
                "scalar to-many path u.orders.total",
            );
        }
        "explain_block_to_many" => {
            assert_explain_contains(
                engine,
                ctx,
                "explain User as u { u.name, orders: u.orders { total } }",
                "nested orders: to-many link u.orders",
            );
        }
        "explain_block_over_to_one" => {
            assert_explain_contains(
                engine,
                ctx,
                "explain Order as o { o.id, u: o.user { .name } }",
                "nested u: to-one link o.user",
            );
        }
        other => panic!(
            "site observation `{other}` is listed in SITES but has no pinned \
             expectation: it would ride along on parity alone, which passes when \
             every ordering is uniformly wrong"
        ),
    }
}

// ---------------------------------------------------------------------------
// The parity assertions
// ---------------------------------------------------------------------------

#[test]
fn every_site_is_identical_across_ddl_orderings() {
    let baseline_order = ORDERS[0];
    let mut baseline_engine = build(baseline_order, &temp_dir("order_baseline"));
    let baseline = observe(&mut baseline_engine);

    for order in ORDERS.iter().skip(1) {
        let mut engine = build(*order, &temp_dir(&format!("order_{order:?}")));
        let observed = observe(&mut engine);
        assert_eq!(baseline.len(), observed.len(), "probe list drifted");
        for ((label, want), (other_label, got)) in baseline.iter().zip(&observed) {
            assert_eq!(label, other_label, "probe list drifted between runs");
            assert_eq!(
                want, got,
                "site observation `{label}` differs between DDL order \
                 {baseline_order:?} and {order:?}, but both reach the same final \
                 schema. Cardinality is derived from the schema, so declaration \
                 order cannot change it."
            );
        }
    }
}

/// Every enumerated observation, in every ordering, pinned to the answer the
/// current schema implies.
#[test]
fn every_site_reports_what_the_current_schema_implies() {
    for order in ORDERS {
        let mut engine = build(order, &temp_dir(&format!("pinned_{order:?}")));
        for site in SITES {
            assert_pinned(
                &mut engine,
                &format!("{order:?}/{}/{}", site.site, site.label),
                site.label,
            );
        }
        // `describe` must not contradict itself: `User.id` is unique, so the
        // column row and every link row onto it have to agree in one output.
        let user = describe_index_column(&mut engine, "User");
        assert!(
            user.iter().any(|(c, i)| c == "id" && i == "unique"),
            "{order:?}: describe User index column: {user:?}"
        );
        assert!(
            user.iter()
                .any(|(c, i)| c == "Order.user" && i == "<- Order (to-one, user_id -> id)"),
            "{order:?}: describe User incoming link row: {user:?}"
        );
    }
}

#[test]
fn every_site_is_identical_after_reopen() {
    for order in ORDERS {
        let dir = temp_dir(&format!("reopen_{order:?}"));
        let before = {
            let mut engine = build(order, &dir);
            observe(&mut engine)
        };
        let after = {
            let mut engine = Engine::new(&dir).unwrap();
            observe(&mut engine)
        };
        for ((label, want), (_, got)) in before.iter().zip(&after) {
            assert_eq!(
                want, got,
                "{order:?}: site observation `{label}` changed across a reopen"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The advisory byte: stale by design, and read by nothing
// ---------------------------------------------------------------------------

/// The premise every mutation test in this file rests on. `LinkDef::kind` is
/// **not** kept in step with the schema: after `link` then `alter add unique`
/// the byte says to-many while the truth is to-one. If a future change quietly
/// reintroduces a resync, this fails, and that is the point: the resync is what
/// made the five sites untestable, because it answered before they did.
#[test]
fn the_advisory_byte_goes_stale_and_the_engine_ignores_it() {
    let dir = temp_dir("advisory");
    {
        let mut engine = build(Order::LinkThenUnique, &dir);
        // Every surface reports the truth.
        assert_eq!(link_cardinalities(&mut engine), expected_cardinalities());
    }

    let catalog = Catalog::open_read_only(&dir).unwrap();
    assert_eq!(
        catalog.link("Order", "user").unwrap().kind,
        LinkKind::ToMany,
        "the advisory byte is a declare-time record and must NOT be refreshed; \
         if this now says ToOne, a resync came back and the five live sites have \
         lost their test coverage"
    );
    assert_eq!(
        catalog.link_kind("Order", "user"),
        Some(LinkKind::ToOne),
        "the derived answer is the truth"
    );
    assert_eq!(
        catalog.derive_link_kind("User", "id"),
        LinkKind::ToOne,
        "derivation reads index uniqueness live"
    );
}

/// CRC-32/ISO-HDLC, the variant `crc32fast` computes. Validated against the
/// checksum the engine itself wrote before anything is patched, so a wrong
/// implementation fails loudly instead of silently producing an unreadable
/// file.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Overwrite the single link's kind byte in `catalog.bin`, fixing up the
/// trailing CRC, and return what was there before. `encode_links_section`
/// writes each link's kind as the last byte it emits and the section is the
/// last thing before the CRC, so with one link the kind is the final payload
/// byte.
fn set_only_link_kind_byte(dir: &std::path::Path, write: u8) -> u8 {
    let catalog_path = dir.join("catalog.bin");
    let mut bytes = std::fs::read(&catalog_path).unwrap();
    let payload_len = bytes.len() - 4;
    let stored_crc = u32::from_le_bytes(bytes[payload_len..].try_into().unwrap());
    assert_eq!(
        crc32(&bytes[..payload_len]),
        stored_crc,
        "checksum helper disagrees with the engine, so the patch below would be junk"
    );
    let kind_byte = payload_len - 1;
    let previous = bytes[kind_byte];
    assert!(
        previous <= 1,
        "expected the single link's kind byte (0 or 1) as the last payload byte, got {previous}"
    );
    bytes[kind_byte] = write;
    let patched_crc = crc32(&bytes[..payload_len]);
    bytes[payload_len..].copy_from_slice(&patched_crc.to_le_bytes());
    std::fs::write(&catalog_path, &bytes).unwrap();
    previous
}

fn only_link_kind_byte(dir: &std::path::Path) -> u8 {
    let bytes = std::fs::read(dir.join("catalog.bin")).unwrap();
    bytes[bytes.len() - 5]
}

const SMALL_FIXTURE: &[&str] = &[
    "type User { required unique id: int, required name: str }",
    "type Order { required id: int, user_id: int }",
    "link Order.user -> User on user_id = id",
    r#"insert User { id := 1, name := "alice" }"#,
    "insert Order { id := 1, user_id := 1 }",
];

/// A catalog on disk whose kind byte says the opposite of the truth must change
/// nothing, and must still say the opposite after the open: no repair pass, no
/// rewrite, no read.
#[test]
fn a_wrong_byte_on_disk_changes_no_surface_and_is_not_repaired() {
    let dir = temp_dir("wrong_byte");
    {
        let mut engine = Engine::new(&dir).unwrap();
        for stmt in SMALL_FIXTURE {
            engine.execute_powql(stmt).unwrap();
        }
    }

    // 0 = ToOne, which is what the unique-then-link fixture wrote.
    assert_eq!(set_only_link_kind_byte(&dir, 1), 0);

    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(
        link_cardinalities(&mut engine),
        vec![(
            "Order".to_string(),
            "user".to_string(),
            "to-one".to_string()
        )],
        "a wrong byte on disk must not reach `schema links`"
    );
    assert_eq!(
        describe_row(&mut engine, "Order", "user"),
        "-> User (to-one, user_id -> id)"
    );
    assert_eq!(
        describe_row(&mut engine, "User", "Order.user"),
        "<- Order (to-one, user_id -> id)"
    );
    let rows = rows_of(&mut engine, "Order as o { o.id, o.user.name }");
    assert_eq!(text_of(&rows[0][1]), "alice");
    assert!(engine
        .execute_powql("Order as o { o.id, u: o.user { .name } }")
        .is_err());
    drop(engine);

    assert_eq!(
        only_link_kind_byte(&dir),
        1,
        "nothing may rewrite the advisory byte: a repair-on-open path is the \
         mechanism this fix deleted"
    );
}

/// The round-1 regression, pinned. `Catalog::open_inner` replays the WAL before
/// anything else, and `replay_records` persists the catalog when it saw a DDL
/// record. A consistency assertion over the kind byte therefore fired *during
/// crash recovery*, so a database with a stale byte and pending WAL records
/// (that is: any `kill -9`, PowDB's designed recovery path) aborted on open in
/// a debug build. The byte is no longer a correctness input and no invariant is
/// asserted over it, so recovery is ordinary.
#[test]
fn a_hard_crash_with_a_stale_byte_and_pending_wal_records_reopens() {
    let dir = temp_dir("crash_stale");
    {
        let mut engine = Engine::new(&dir).unwrap();
        for stmt in [
            "type User { required id: int, required name: str }",
            "type Order { required id: int, user_id: int }",
            // Link first: the advisory byte is written as to-many...
            "link Order.user -> User on user_id = id",
            r#"insert User { id := 1, name := "alice" }"#,
            "insert Order { id := 1, user_id := 1 }",
            // ...and this DDL, which makes the truth to-one, is the pending WAL
            // record that replay applies on the next open.
            "alter User add unique .id",
            "insert Order { id := 2, user_id := 1 }",
        ] {
            engine.execute_powql(stmt).unwrap();
        }
        std::mem::forget(engine); // hard crash: no Drop, no checkpoint
    }

    // Force the two preconditions the regression needed, rather than hoping the
    // crash happened to leave them. (1) The byte on disk says ToMany while the
    // truth is ToOne, which is the state of every database written by the
    // declare-order-dependent versions. Writing it here rather than asserting
    // it keeps the test faithful under a variant that repairs the byte at DDL
    // time: such a variant would leave 0 here, and the reopen is what has to be
    // exercised, not the write path.
    set_only_link_kind_byte(&dir, 1);
    assert_eq!(only_link_kind_byte(&dir), 1);
    // (2) The WAL still holds un-replayed records, so `replay_records` runs and
    // persists the catalog during recovery.
    let wal_len = std::fs::metadata(dir.join("wal.log")).unwrap().len();
    assert!(
        wal_len > 8,
        "the crash must leave records past the 8-byte PWAL header for replay to \
         run at all, got {wal_len} bytes"
    );
    assert!(
        Catalog::open_read_only(&dir).is_err(),
        "a read-only open refuses a directory with un-checkpointed writes; if it \
         succeeds here the WAL is empty and this test proves nothing"
    );

    let mut engine = Engine::new(&dir).unwrap();
    assert_eq!(
        link_cardinalities(&mut engine),
        vec![(
            "Order".to_string(),
            "user".to_string(),
            "to-one".to_string()
        )]
    );
    let rows = rows_of(&mut engine, "Order as o { o.id, o.user.name }");
    assert_eq!(rows.len(), 2, "the replayed insert survived recovery");
    assert_eq!(text_of(&rows[0][1]), "alice");
}

// ---------------------------------------------------------------------------
// Closing the enumeration from the source side
// ---------------------------------------------------------------------------

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/query has two ancestors")
        .to_path_buf()
}

fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The behavioural probes above can only catch a violation on a surface they
/// reach. This closes the other end: in engine source, a `.kind` field access
/// is forbidden in any file that knows what a `LinkDef` is, with exactly one
/// allowed exception (writing the byte out). A sixth site that consults the
/// advisory byte fails here even if no query in this file would reach it.
#[test]
fn no_engine_source_reads_the_advisory_byte() {
    let root = repo_root();
    let mut files = Vec::new();
    rust_sources(&root.join("crates/query/src"), &mut files);
    rust_sources(&root.join("crates/storage/src"), &mut files);
    files.sort();
    assert!(
        files.len() > 10,
        "source walk found almost nothing: {files:?}"
    );

    // The one legitimate touch: serializing the advisory byte into the v7
    // catalog layout. It makes no decision.
    const ALLOWED: &[&str] = &["out.push(link.kind.to_u8());"];

    let mut inspected: Vec<String> = Vec::new();
    let mut offenders: Vec<String> = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        // Only files that can hold a `LinkDef` can read a link's kind.
        if !text.contains("LinkDef") && !text.contains("LinkKind") {
            continue;
        }
        inspected.push(
            path.strip_prefix(&root)
                .unwrap_or(path)
                .display()
                .to_string(),
        );
        // Unit tests live at the bottom of these files in a top-level
        // `#[cfg(test)] mod tests` and may assert on the byte; the rule is
        // about production decisions. Cut at that module specifically, not at
        // the first `#[cfg(test)]` in the file: `catalog.rs` has a test-only
        // failpoint helper near the top, and cutting there would have excluded
        // ~4000 lines of production code from the scan (it did, until a
        // deliberately planted violation in `drop_link` went unnoticed).
        let production = match text.find("\n#[cfg(test)]\nmod ") {
            Some(idx) => &text[..idx],
            None => &text[..],
        };
        assert!(
            production.len() * 2 > text.len(),
            "{}: the production slice is under half the file, so the test-module \
             cut point is wrong and most of the file is not being scanned",
            path.display()
        );
        for (n, line) in production.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("").trim();
            // A field access, not the `io::Error::kind()` method. Strip the
            // method calls first rather than skipping the whole line when one
            // appears: a line can hold both, and skipping would hide the read.
            if !code.replace(".kind()", "").contains(".kind") {
                continue;
            }
            if ALLOWED.iter().any(|a| code.contains(a)) {
                continue;
            }
            offenders.push(format!(
                "{}:{}: {code}",
                path.strip_prefix(&root).unwrap_or(path).display(),
                n + 1
            ));
        }
    }
    inspected.sort();
    assert_eq!(
        inspected,
        vec![
            "crates/query/src/executor/plan_exec/dispatch.rs",
            "crates/query/src/executor/plan_exec/lowering.rs",
            "crates/query/src/executor/plan_exec/scan.rs",
            "crates/storage/src/catalog.rs",
        ],
        "the set of engine files that know what a link's cardinality is has \
         changed. That set IS the enumeration this file tests, so add the new \
         file's site to SITES with a probe before updating this list"
    );
    assert!(
        offenders.is_empty(),
        "engine source reads the advisory `LinkDef::kind` byte. Cardinality must \
         come from `Catalog::derive_link_kind` / `Catalog::link_kind`, which read \
         index uniqueness live; the byte is a declare-time record that goes \
         stale. Offending lines:\n{}",
        offenders.join("\n")
    );
}

// ---------------------------------------------------------------------------
// The other direction: a target key that is genuinely not unique
// ---------------------------------------------------------------------------

/// The whole refusal text, not a substring of it. The message is assembled from
/// a shared frame plus a per-case clause, and a clause that does not fit the
/// frame produces a sentence that does not parse: asserting only that a few
/// words are present is exactly how the ungrammatical version shipped.
const TO_MANY_NO_INDEX: &str = "link `user` on type `Order` is a to-many link: its target key \
     `User.id` is not unique, so a hop can match many rows. To read one value \
     per row, make the target key unique with `alter User add unique .id`. To \
     read every match, traverse it with a block (`user: o.user { ... }`)";

const TO_MANY_PLAIN_INDEX: &str = "link `user` on type `Order` is a to-many link: its target key \
     `User.id` is not unique, so a hop can match many rows. There is no way to \
     read one value per row here: `User.id` already carries a non-unique index \
     and an index cannot be upgraded in place, so this link stays to-many. To \
     read every match, traverse it with a block (`user: o.user { ... }`)";

/// A non-unique target key stays to-many under either DDL order, and the
/// refusal leads with the remedy that keeps the query as written. The block
/// form is the alternative, not the default: it turns a foreign-key lookup into
/// a one-element array the caller unwraps forever.
#[test]
fn a_genuinely_non_unique_target_key_reports_the_remedy_that_works() {
    let mut engine = Engine::new(&temp_dir("remedy")).unwrap();
    for stmt in [
        "type User { required id: int, required name: str }",
        "type Order { required id: int, user_id: int }",
        "link Order.user -> User on user_id = id",
        r#"insert User { id := 1, name := "alice" }"#,
        "insert Order { id := 1, user_id := 1 }",
    ] {
        engine.execute_powql(stmt).unwrap();
    }

    assert_eq!(
        link_cardinalities(&mut engine),
        vec![(
            "Order".to_string(),
            "user".to_string(),
            "to-many".to_string()
        )]
    );
    let err = engine
        .execute_powql("Order as o { o.id, o.user.name }")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(TO_MANY_NO_INDEX),
        "refusal text drifted.\nwant: {TO_MANY_NO_INDEX}\ngot:  {err}"
    );

    // The remedy the message names is the remedy that works, with no
    // re-declaration of the link.
    engine.execute_powql("alter User add unique .id").unwrap();
    assert_eq!(
        link_cardinalities(&mut engine),
        vec![(
            "Order".to_string(),
            "user".to_string(),
            "to-one".to_string()
        )]
    );
    let rows = rows_of(&mut engine, "Order as o { o.id, o.user.name }");
    assert_eq!(text_of(&rows[0][1]), "alice");
}

/// A target key carrying a *plain* index cannot be upgraded in place today, so
/// `alter T add unique .c` is refused. The refusal must not point at it: naming
/// a statement that errors is the same failure as naming a statement that does
/// not exist. Both orderings of "plain index" and "link" agree.
#[test]
fn plain_index_orderings_agree_and_stay_to_many() {
    let scripts: [(&str, Vec<&str>); 2] = [
        (
            "index_then_link",
            vec![
                "type User { required id: int, required name: str }",
                "type Order { required id: int, user_id: int }",
                "alter User add index .id",
                "link Order.user -> User on user_id = id",
            ],
        ),
        (
            "link_then_index",
            vec![
                "type User { required id: int, required name: str }",
                "type Order { required id: int, user_id: int }",
                "link Order.user -> User on user_id = id",
                "alter User add index .id",
            ],
        ),
    ];

    let mut observed: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for (name, script) in scripts {
        let mut engine = Engine::new(&temp_dir(&format!("plain_{name}"))).unwrap();
        for stmt in &script {
            engine
                .execute_powql(stmt)
                .unwrap_or_else(|e| panic!("{name}: `{stmt}` failed: {e}"));
        }
        engine
            .execute_powql(r#"insert User { id := 1, name := "alice" }"#)
            .unwrap();
        engine
            .execute_powql("insert Order { id := 1, user_id := 1 }")
            .unwrap();

        assert_eq!(
            link_cardinalities(&mut engine),
            vec![(
                "Order".to_string(),
                "user".to_string(),
                "to-many".to_string()
            )],
            "{name}: a plain index is not uniqueness"
        );
        let err = engine
            .execute_powql("Order as o { o.id, o.user.name }")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(TO_MANY_PLAIN_INDEX),
            "{name}: refusal text drifted.\nwant: {TO_MANY_PLAIN_INDEX}\ngot:  {err}"
        );
        assert!(
            !err.contains("add unique"),
            "{name}: must not point at `add unique`, which this state refuses: {err}"
        );
        // The engine's own claim, checked against the engine's own DDL.
        let refused = engine
            .execute_powql("alter User add unique .id")
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("already indexed"),
            "{name}: expected the in-place upgrade to be refused, got: {refused}"
        );

        observed.push((
            name.to_string(),
            [
                "Order as o { o.id, o.user.name }",
                "schema links",
                "describe Order",
                "describe User",
            ]
            .iter()
            .map(|q| ((*q).to_string(), outcome(&mut engine, q)))
            .collect(),
        ));
    }
    assert_eq!(
        observed[0].1, observed[1].1,
        "plain-index orderings reach the same schema and must observe the same thing"
    );
}

// ---------------------------------------------------------------------------
// Every catalog entry point that can move index uniqueness
// ---------------------------------------------------------------------------

fn col(name: &str, type_id: TypeId, position: u16) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        type_id,
        required: true,
        position,
    }
}

fn schema(table: &str, columns: Vec<ColumnDef>) -> Schema {
    Schema {
        table_name: table.into(),
        columns,
    }
}

fn live_kind(catalog: &Catalog, owner: &str, name: &str) -> LinkKind {
    catalog
        .link_kind(owner, name)
        .unwrap_or_else(|| panic!("link {owner}.{name} missing"))
}

/// The catalog-side enumeration: every entry point that can move
/// `is_index_unique(target_type, target_key)` must be visible to the next read,
/// including the ones that cannot move it today. A site that is inert now is
/// exactly the site a future change turns into the next partial application, so
/// it is asserted rather than reasoned about. Nothing here has to be *called*
/// after a mutation any more, which is the point of deleting the cache: the
/// derivation runs at read time, so an entry point cannot forget to invoke it.
#[test]
fn every_uniqueness_mutating_catalog_entry_point_is_visible_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::create(dir.path()).unwrap();

    // create_table / create_table_full.
    catalog
        .create_table(schema(
            "User",
            vec![
                col("id", TypeId::Int, 0),
                col("name", TypeId::Str, 1),
                col("doc", TypeId::Json, 2),
            ],
        ))
        .unwrap();
    catalog
        .create_table(schema(
            "Order",
            vec![col("id", TypeId::Int, 0), col("user_id", TypeId::Int, 1)],
        ))
        .unwrap();

    catalog
        .create_link(LinkDef {
            owner_type: "Order".into(),
            name: "user".into(),
            target_type: "User".into(),
            local_key: "user_id".into(),
            target_key: "id".into(),
            // Ignored on create: the catalog derives the real value. Passing
            // the wrong one on purpose proves the caller cannot pin it.
            kind: LinkKind::ToOne,
        })
        .unwrap();
    assert_eq!(
        live_kind(&catalog, "Order", "user"),
        LinkKind::ToMany,
        "no unique index on User.id yet"
    );

    // create_index (non-unique) must not promote the link.
    catalog.create_index("User", "id").unwrap();
    assert_eq!(
        live_kind(&catalog, "Order", "user"),
        LinkKind::ToMany,
        "a plain index is not uniqueness"
    );
    // Indexing an unrelated column changes nothing.
    catalog.create_index("User", "name").unwrap();
    assert_eq!(live_kind(&catalog, "Order", "user"), LinkKind::ToMany);

    // create_index_unique on the target key: THE LIVE BUG. The link was
    // declared before this ran and must still read as to-one.
    let dir2 = tempfile::tempdir().unwrap();
    let mut catalog2 = Catalog::create(dir2.path()).unwrap();
    catalog2
        .create_table(schema(
            "User",
            vec![
                col("id", TypeId::Int, 0),
                col("name", TypeId::Str, 1),
                col("doc", TypeId::Json, 2),
            ],
        ))
        .unwrap();
    catalog2
        .create_table(schema(
            "Order",
            vec![col("id", TypeId::Int, 0), col("user_id", TypeId::Int, 1)],
        ))
        .unwrap();
    catalog2
        .create_link(LinkDef {
            owner_type: "Order".into(),
            name: "user".into(),
            target_type: "User".into(),
            local_key: "user_id".into(),
            target_key: "id".into(),
            kind: LinkKind::ToOne,
        })
        .unwrap();
    assert_eq!(live_kind(&catalog2, "Order", "user"), LinkKind::ToMany);
    catalog2.create_index_unique("User", "id", true).unwrap();
    assert_eq!(
        live_kind(&catalog2, "Order", "user"),
        LinkKind::ToOne,
        "adding the unique index after the link must be visible immediately"
    );
    assert_eq!(
        catalog2.link("Order", "user").unwrap().kind,
        LinkKind::ToMany,
        "and the advisory byte must NOT have been rewritten to match"
    );

    // Expression index create/drop. A link target key is always a stored
    // column, so an expression index can never back one: inert, asserted.
    let json_path = StoredJsonPathV1::new("doc", vec![StoredJsonPathSegmentV1::Key("k".into())]);
    let index_id = catalog2
        .create_expression_index_metadata("User", 1, json_path.canonical_text(), json_path, true)
        .unwrap();
    assert_eq!(live_kind(&catalog2, "Order", "user"), LinkKind::ToOne);
    catalog2.drop_expression_index("User", index_id).unwrap();
    assert_eq!(live_kind(&catalog2, "Order", "user"), LinkKind::ToOne);

    // Add column, and the drop guards that keep a link from being orphaned.
    catalog2
        .alter_table_add_column(
            "User",
            ColumnDef {
                name: "nickname".into(),
                type_id: TypeId::Str,
                required: false,
                position: 3,
            },
        )
        .unwrap();
    assert_eq!(live_kind(&catalog2, "Order", "user"), LinkKind::ToOne);

    // The drop guards name a remedy that exists. PowQL has no `drop link`
    // statement, so pointing a PowQL user at one was pointing at nothing.
    let col_err = catalog2
        .alter_table_drop_column("User", "id")
        .unwrap_err()
        .to_string();
    assert!(
        col_err.contains("Catalog::drop_link(\"Order\", \"user\")")
            && col_err.contains("PowQL has no statement that removes a link")
            && !col_err.contains("drop the link first"),
        "drop-column guard must name a remedy that exists: {col_err}"
    );
    let table_err = catalog2.drop_table("User").unwrap_err().to_string();
    assert!(
        table_err.contains("Catalog::drop_link(\"Order\", \"user\")")
            && table_err.contains("PowQL has no statement that removes a link")
            && !table_err.contains("drop the link first"),
        "drop-table guard must name a remedy that exists: {table_err}"
    );

    // Creating and dropping an unrelated table changes nothing.
    catalog2
        .create_table(schema("Audit", vec![col("id", TypeId::Int, 0)]))
        .unwrap();
    assert_eq!(live_kind(&catalog2, "Order", "user"), LinkKind::ToOne);
    catalog2.drop_table("Audit").unwrap();
    assert_eq!(live_kind(&catalog2, "Order", "user"), LinkKind::ToOne);

    // The reverse transition (to-one -> to-many). Dropping a stored-column
    // index is refused today, which is the only reason it cannot happen; the
    // derivation already handles it, and this pins the contract a future lane
    // has to meet.
    assert_eq!(
        catalog2.derive_link_kind("User", "id"),
        LinkKind::ToOne,
        "derivation reads uniqueness live, so a future index drop flips it"
    );

    // The answer survives a reopen because it is recomputed, not stored.
    drop(catalog2);
    let reopened = Catalog::open(dir2.path()).unwrap();
    assert_eq!(live_kind(&reopened, "Order", "user"), LinkKind::ToOne);
    assert_eq!(
        reopened.link("Order", "user").unwrap().kind,
        LinkKind::ToMany,
        "the reopen must not repair the advisory byte either"
    );
}

// ---------------------------------------------------------------------------
// Paths that were already live, and one that could panic
// ---------------------------------------------------------------------------

/// `build_scalar_link_maps` already read uniqueness live to pick between a
/// point probe and a full scan. The derivation change must not regress it: a
/// link promoted to to-one after declaration has to answer identically whether
/// the executor probes or scans.
#[test]
fn a_promoted_link_answers_the_same_through_probes_and_scans() {
    let mut engine = Engine::new(&temp_dir("probe_parity")).unwrap();
    for stmt in [
        "type User { required id: int, required name: str }",
        "type Order { required id: int, user_id: int }",
        "link Order.user -> User on user_id = id",
    ] {
        engine.execute_powql(stmt).unwrap();
    }
    for i in 1..=40 {
        engine
            .execute_powql(&format!(r#"insert User {{ id := {i}, name := "u{i}" }}"#))
            .unwrap();
        engine
            .execute_powql(&format!("insert Order {{ id := {i}, user_id := {i} }}"))
            .unwrap();
    }
    engine.execute_powql("alter User add unique .id").unwrap();

    // Selective (probe-friendly) and unselective (scan) shapes must agree.
    let one = rows_of(
        &mut engine,
        "Order as o filter o.id = 7 { o.id, o.user.name }",
    );
    assert_eq!(text_of(&one[0][1]), "u7");
    let all = rows_of(&mut engine, "Order as o { o.id, o.user.name }");
    assert_eq!(all.len(), 40);
    assert_eq!(text_of(&all[6][1]), "u7");
}

/// A link path is always written with at least one hop, but the resolver runs
/// on whatever plan it is handed. `panic = "abort"` makes any reachable panic
/// on a request path a remote DoS, so an empty hop chain has to be a typed
/// error. This asserts the shapes that reach the resolver stay well-formed and
/// error cleanly rather than aborting.
#[test]
fn malformed_link_paths_are_typed_errors_not_panics() {
    let mut engine = Engine::new(&temp_dir("malformed")).unwrap();
    for stmt in [
        "type User { required unique id: int, required name: str }",
        "type Order { required id: int, user_id: int }",
        "link Order.user -> User on user_id = id",
    ] {
        engine.execute_powql(stmt).unwrap();
    }
    for query in [
        "Order as o { o.id, o.nosuch.name }",
        "Order as o { o.id, o.user.nosuch }",
        "Order as o { o.id, o.user.nosuch.name }",
        "Order as o { o.id, u: o.nosuch { .name } }",
    ] {
        let err = engine
            .execute_powql(query)
            .expect_err(&format!("`{query}` must error"));
        let text = err.to_string();
        assert!(
            !text.is_empty() && !text.contains("panicked"),
            "`{query}` must fail as a typed error: {text}"
        );
    }
}
