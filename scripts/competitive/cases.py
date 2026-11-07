"""Versioned equivalent-output workloads, with explicit language mappings."""

import math

PROFILE = "relational-reads-v1"


def case(name, category, sql, powql, iterations=50, **extra):
    return dict(id=name, category=category, sql=sql, powql=powql,
                iterations=iterations, deliveries=["materialized"], **extra)


CASES = [
    case("point_bound", "point_bound", "SELECT id,name,amount FROM BenchRow WHERE id=$ID",
         'BenchRow filter .id = $ID { .id, .name, .amount }', 1000, prepared=True),
    case("point_indexed", "point", "SELECT id,name,amount FROM BenchRow WHERE id=$ID",
         'BenchRow filter .id = $ID { .id, .name, .amount }', 1000),
    case("point_missing", "point", "SELECT id,name FROM BenchRow WHERE id=$MISS",
         'BenchRow filter .id = $MISS { .id, .name }', 1000),
    case("point_unindexed", "scan", "SELECT id,amount FROM BenchRow WHERE name='user_$ID'",
         'BenchRow filter .name = "user_$ID" { .id, .amount }', 20),
    case("range_page", "range", "SELECT id,amount FROM BenchRow WHERE id>$ID ORDER BY id LIMIT 20",
         'BenchRow filter .id > $ID order .id limit 20 { .id, .amount }'),
    case("filtered_count", "scan", "SELECT COUNT(*) FROM BenchRow WHERE score>=975",
         'count(BenchRow filter .score >= 975)'),
    case("and_projection", "combined", "SELECT id,name FROM BenchRow WHERE score>=950 AND amount>0 ORDER BY id LIMIT 100",
         'BenchRow filter .score >= 950 and .amount > 0 order .id limit 100 { .id, .name }'),
    case("or_projection", "combined", "SELECT id,name FROM BenchRow WHERE score>990 OR grp=3 ORDER BY id LIMIT 100",
         'BenchRow filter .score > 990 or .grp = 3 order .id limit 100 { .id, .name }'),
    case("indexed_in", "membership", "SELECT id FROM BenchRow WHERE bucket IN (5,17,31) ORDER BY id",
         'BenchRow filter .bucket in (5,17,31) order .id { .id }'),
    case("unindexed_in", "membership", "SELECT id FROM BenchRow WHERE score IN (5,17,31) ORDER BY id",
         'BenchRow filter .score in (5,17,31) order .id { .id }'),
    case("unindexed_not_in", "membership", "SELECT COUNT(*) FROM BenchRow WHERE score NOT IN (5,17,31)",
         'count(BenchRow filter .score not in (5,17,31))'),
    case("null_count", "nulls", "SELECT COUNT(*) FROM BenchRow WHERE optional_value IS NULL",
         'count(BenchRow filter .optional_value = null)'),
    case("sort_top100", "sorting", "SELECT id,score FROM BenchRow WHERE amount>10 ORDER BY score DESC,id ASC LIMIT 100",
         'BenchRow filter .amount > 10 order .score desc, .id asc limit 100 { .id, .score }'),
    case("pagination_offset", "pagination", "SELECT id,score FROM BenchRow ORDER BY score,id LIMIT 100 OFFSET $OFFSET",
         'BenchRow order .score, .id limit 100 offset $OFFSET { .id, .score }', 20),
    case("pagination_keyset", "pagination", "SELECT id,score FROM BenchRow WHERE score>$KEY OR (score=$KEY AND id>$LAST) ORDER BY score,id LIMIT 100",
         'BenchRow filter .score > $KEY or (.score = $KEY and .id > $LAST) order .score, .id limit 100 { .id, .score }', 20),
    case("pagination_pk_offset", "pagination", "SELECT id,score FROM BenchRow ORDER BY id LIMIT 100 OFFSET $OFFSET",
         'BenchRow order .id limit 100 offset $OFFSET { .id, .score }', 20),
    case("pagination_pk_keyset", "pagination", "SELECT id,score FROM BenchRow WHERE id>$PKLAST ORDER BY id LIMIT 100",
         'BenchRow filter .id > $PKLAST order .id limit 100 { .id, .score }', 20),
    case("sum", "aggregate", "SELECT SUM(amount) FROM BenchRow",
         'sum(BenchRow { .amount })', postgres_sql="SELECT SUM(amount)::BIGINT FROM BenchRow"),
    case("avg", "aggregate", "SELECT AVG(amount) FROM BenchRow",
         'avg(BenchRow { .amount })', postgres_sql="SELECT AVG(amount)::DOUBLE PRECISION FROM BenchRow"),
    case("group_having", "combined", "SELECT grp,COUNT(*) AS n,SUM(amount) AS total FROM BenchRow GROUP BY grp HAVING COUNT(*)>=10 ORDER BY grp",
         'BenchRow group .grp having count(.id) >= 10 order .grp { .grp, n: count(.id), total: sum(.amount) }',
         postgres_sql="SELECT grp,COUNT(*) AS n,SUM(amount)::BIGINT AS total FROM BenchRow GROUP BY grp HAVING COUNT(*)>=10 ORDER BY grp"),
    case("distinct", "set", "SELECT DISTINCT grp FROM BenchRow ORDER BY grp",
         'BenchRow distinct order .grp { .grp }'),
    case("join_projection", "join", "SELECT r.id,g.label,r.amount FROM BenchRow r JOIN BenchGroup g ON r.grp=g.id WHERE r.score>990 ORDER BY r.id LIMIT 100",
         'BenchRow as r inner join BenchGroup as g on r.grp = g.id filter r.score > 990 order r.id limit 100 { r.id, g.label, r.amount }', 20),
    case("compound_join", "combined", "SELECT r.id,g.label FROM BenchRow r JOIN BenchGroup g ON r.grp=g.id AND g.id>10 WHERE r.amount>0 ORDER BY r.id LIMIT 100",
         'BenchRow as r inner join BenchGroup as g on r.grp = g.id and g.id > 10 filter r.amount > 0 order r.id limit 100 { r.id, g.label }', 20),
    case("join_group_having", "combined", "SELECT g.id,COUNT(r.id) AS n,SUM(r.amount) AS total FROM BenchRow r JOIN BenchGroup g ON r.grp=g.id GROUP BY g.id HAVING COUNT(r.id)>=10 ORDER BY g.id",
         'BenchRow as r inner join BenchGroup as g on r.grp = g.id group g.id having count(r.id) >= 10 order g.id { g.id, n: count(r.id), total: sum(r.amount) }', 20,
         postgres_sql="SELECT g.id,COUNT(r.id) AS n,SUM(r.amount)::BIGINT AS total FROM BenchRow r JOIN BenchGroup g ON r.grp=g.id GROUP BY g.id HAVING COUNT(r.id)>=10 ORDER BY g.id"),
    case("exists_correlated", "subquery", "SELECT r.id FROM BenchRow r WHERE EXISTS (SELECT 1 FROM BenchGroup g WHERE g.id=r.grp AND g.id<3) ORDER BY r.id LIMIT 100",
         'BenchRow as r filter exists (BenchGroup as g filter g.id = r.grp and g.id < 3) order r.id limit 100 { r.id }', 20),
    case("in_subquery", "subquery", "SELECT id FROM BenchRow WHERE grp IN (SELECT id FROM BenchGroup WHERE id<3) ORDER BY id LIMIT 100",
         'BenchRow as r filter r.grp in (BenchGroup as g filter g.id < 3 { g.id }) order r.id limit 100 { r.id }', 20),
    case("case_expression", "expression", "SELECT id,CASE WHEN amount>0 THEN 'positive' ELSE 'other' END AS label FROM BenchRow WHERE id<100 ORDER BY id",
         'BenchRow filter .id < 100 order .id { .id, label: case when .amount > 0 then "positive" else "other" end }'),
    case("int64_exact", "types", "SELECT 9007199254740993 AS value FROM BenchRow WHERE id=0",
         'BenchRow filter .id = 0 { value: 9007199254740993 }'),
    case("window_rank", "window", "SELECT id,ROW_NUMBER() OVER (PARTITION BY grp ORDER BY score DESC,id) AS rn FROM BenchRow WHERE id<100 ORDER BY id",
         'BenchRow filter .id < 100 order .id { .id, rn: row_number() over (partition .grp order .score desc, .id) }', 20),
    dict(id="all_rows_narrow", category="delivery", iterations=3,
         sql="SELECT id,amount FROM BenchRow ORDER BY id",
         powql="BenchRow order .id { .id, .amount }",
         deliveries=["materialized", "consume"]),
    dict(id="all_rows_wide", category="delivery", iterations=3,
         sql="SELECT id,name,payload FROM BenchRow ORDER BY id",
         powql="BenchRow order .id { .id, .name, .payload }",
         deliveries=["materialized", "consume"]),
]


def parameters(rows, round_number):
    ordered = sorted(((i * 37) % 1000, i) for i in range(rows))
    offset = min(rows - 100, rows * 4 // 5)
    key, last = ordered[offset - 1]
    stride = (rows // 2 + 104729) % rows
    while math.gcd(stride, rows) != 1:
        stride += 1
    return {"rows": rows, "round": round_number, "offset": offset,
            "key": key, "last": last, "stride": stride}


def query_for(workload, engine, index, params):
    key = "powql" if engine in ("powdb-powql", "powdb-server") else "sql"
    if engine == "postgres" and "postgres_sql" in workload:
        key = "postgres_sql"
    query = workload[key]
    value = id_for(index, params)
    replacements = {"$ID": value, "$MISS": params["rows"] + value,
                    "$OFFSET": params["offset"], "$KEY": params["key"],
                    "$LAST": params["last"], "$PKLAST": params["offset"] - 1}
    for token, number in replacements.items():
        query = query.replace(token, str(number))
    return query


def id_for(index, params):
    return (index * params["stride"] + params["round"] * 7919 + 17) % params["rows"]
