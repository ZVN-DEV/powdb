/**
 * Safe PowQL composition helpers: literal and identifier escaping, plus a
 * tagged-template convenience for building queries without string splicing.
 *
 *     const q = powql`insert ${ident("User")} { name := ${userName}, age := ${age} }`;
 *     await client.query(q);
 *
 * Security model:
 *   - Literals are rendered with quoting/escaping so interpolated values
 *     cannot break out of the surrounding literal.
 *   - Identifiers are validated against `^[A-Za-z_][A-Za-z0-9_]*$`, and are
 *     backtick-quoted when they collide with a PowQL keyword. Anything else
 *     throws a `TypeError`.
 *
 * PowQL string-escape rules (verified against `crates/query/src/lexer.rs`):
 *   - Strings are delimited by `"`.
 *   - `\\` → `\`, `\"` → `"`, `\n` → newline, `\t` → tab.
 *   - For any other `\X`, the backslash is dropped and `X` is kept literally.
 *   - A bare `"` terminates the string, so `"` inside must be `\"` and any
 *     literal backslash must be `\\` (otherwise it would swallow the next char).
 */

const IDENT_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

/**
 * Every word `crates/query/src/lexer.rs` lexes as a keyword rather than an
 * identifier. Unquoted, any of these is a parse error where a table or column
 * name is expected, so {@link escapeIdent} backtick-quotes them.
 *
 * `test/escape.test.ts` diffs this list against the lexer source, so a keyword
 * added to the engine fails the suite here rather than in a user's query.
 */
const RESERVED_WORDS: readonly string[] = [
  "abs", "add", "alter", "and", "as", "asc",
  "auto", "avg", "begin", "between", "case", "cast",
  "ceil", "column", "commit", "concat", "conflict", "count",
  "cross", "date_add", "date_diff", "default", "delete", "dense_rank",
  "desc", "describe", "distinct", "drop", "else", "end",
  "exists", "explain", "extract", "false", "filter", "floor",
  "group", "having", "in", "index", "inner", "insert",
  "is", "join", "json_text", "json_type", "left", "length",
  "let", "like", "limit", "link", "lower", "match",
  "materialize", "materialized", "max", "min", "multi", "not",
  "now", "null", "offset", "on", "or", "order",
  "outer", "over", "partition", "pow", "rank", "raw",
  "refresh", "required", "returning", "right", "rollback", "round",
  "row_number", "schema", "select", "sqrt", "substring", "sum",
  "then", "transaction", "trim", "true", "type", "union",
  "unique", "update", "upper", "upsert", "view", "when",
];

const RESERVED = new Set(RESERVED_WORDS);

/** The reserved-word table {@link escapeIdent} quotes against. */
export function powqlReservedWords(): readonly string[] {
  return RESERVED_WORDS;
}

/** Inclusive bounds of a PowDB `int`, which is a signed 64-bit integer. */
const INT_MIN = -(2n ** 63n);
const INT_MAX = 2n ** 63n - 1n;

/**
 * Render a bigint, refusing one the engine cannot hold. An out-of-range value
 * would otherwise be spliced into the query text as digits the engine rejects
 * at parse time, with nothing pointing back at the call that produced it.
 */
function renderBigint(fn: string, value: bigint): string {
  if (value < INT_MIN || value > INT_MAX) {
    throw new TypeError(
      `${fn}: bigint ${value} is outside the signed 64-bit range PowDB stores`
    );
  }
  return value.toString(10);
}

/** Wrapper marking a string as an identifier (vs a literal) for `powql` tagged templates. */
export class PowqlIdent {
  constructor(public readonly name: string) {}
}

/** Factory for {@link PowqlIdent}. Prefer this over `new PowqlIdent(...)` at call sites. */
export function ident(name: string): PowqlIdent {
  return new PowqlIdent(name);
}

/**
 * Render a PowQL identifier (table name, field name, alias). Returns the
 * identifier unchanged, or backtick-quoted when it collides with a PowQL
 * keyword — `escapeIdent("select")` gives `` `select` ``, which the lexer
 * reads as a plain identifier. Throws `TypeError` on any invalid input
 * (non-string, empty, or containing characters outside
 * `[A-Za-z_][A-Za-z0-9_]*`). Pass the bare name: a value that already carries
 * backticks is rejected rather than quoted twice.
 */
export function escapeIdent(name: string): string {
  if (typeof name !== "string") {
    throw new TypeError(
      `escapeIdent: expected string, got ${typeof name}`
    );
  }
  if (name.length === 0) {
    throw new TypeError("escapeIdent: identifier must not be empty");
  }
  if (!IDENT_RE.test(name)) {
    const hint = name.includes("`")
      ? " (pass the bare name; escapeIdent adds the backticks a keyword needs)"
      : "";
    throw new TypeError(
      `escapeIdent: invalid identifier ${JSON.stringify(name)} (must match /^[A-Za-z_][A-Za-z0-9_]*$/)${hint}`
    );
  }
  return RESERVED.has(name) ? `\`${name}\`` : name;
}

/**
 * Render a JS value as a PowQL literal. Supports `string`, `number`, `bigint`,
 * `boolean`, and `null`. Rejects `NaN`/`±Infinity`, `undefined`, symbols,
 * objects, and arrays with `TypeError`.
 *
 * - string → `"..."` with `\` and `"` backslash-escaped (C-style, per the
 *   PowQL lexer). Backslash must be escaped first to avoid double-processing.
 * - number → decimal; rejects non-finite
 * - bigint → decimal digits; rejects anything outside the signed 64-bit range
 * - boolean → `true` / `false`
 * - null   → `null`
 */
export function escapeLiteral(
  value: string | number | bigint | boolean | null
): string {
  if (value === null) return "null";

  const t = typeof value;

  if (t === "string") {
    const escaped = (value as string)
      .replace(/\\/g, "\\\\")
      .replace(/"/g, '\\"');
    return `"${escaped}"`;
  }

  if (t === "number") {
    const n = value as number;
    if (!Number.isFinite(n)) {
      throw new TypeError(
        `escapeLiteral: non-finite number ${String(n)} cannot be represented as a PowQL literal`
      );
    }
    return String(n);
  }

  if (t === "bigint") {
    return renderBigint("escapeLiteral", value as bigint);
  }

  if (t === "boolean") {
    return value ? "true" : "false";
  }

  throw new TypeError(`escapeLiteral: unsupported type ${describe(value)}`);
}

/**
 * Tagged template for safe PowQL composition. Each interpolated value is
 * escaped as a literal by default; wrap it in `ident(...)` to interpolate as
 * an identifier.
 *
 *     const q = powql`insert ${ident(table)} { name := ${userName} }`;
 */
export function powql(
  strings: TemplateStringsArray,
  ...values: unknown[]
): string {
  let out = strings[0] ?? "";
  for (let i = 0; i < values.length; i++) {
    out += renderInterpolation(values[i]);
    out += strings[i + 1] ?? "";
  }
  return out;
}

// ──────────────────────────────────────────────────────────
// SQL frontend
// ──────────────────────────────────────────────────────────

/**
 * SQL composition helpers for PowDB's SQL frontend.
 *
 * READ THIS FIRST. PowDB's SQL surface has NO parameter binding on any wire
 * frame: `querySql`/`querySqlNative` take a statement and nothing else, and
 * there is no `QuerySqlParams` message. The PowQL surface does have real `$N`
 * binding (`client.query(q, [values])`), where the server substitutes a literal
 * TOKEN and injection-shaped input is inert.
 *
 * So: **prefer PowQL with `$N` parameters for anything built from untrusted
 * input.** These helpers exist because the SQL path would otherwise leave
 * string concatenation as the only option, which is strictly worse. They escape
 * correctly for PowDB's own SQL lexer (`crates/query/src/sql.rs`), but escaping
 * is a weaker guarantee than binding: it depends on the value landing in a
 * string/number position, and it cannot make an identifier or a keyword safe.
 *
 * PowDB SQL string rules (verified against `lex_sql` in `crates/query/src/sql.rs`):
 *   - Strings are delimited by `'`.
 *   - `''` inside a single-quoted string is a literal `'`.
 *   - A backslash also escapes the next character (`\n`, `\t`, and `\X` → `X`),
 *     which standard SQL does NOT do, so a literal backslash must be doubled.
 *   - `"` also opens a string in this lexer, so double quotes cannot be used to
 *     quote an identifier. Identifiers are therefore validated, never quoted.
 */

/**
 * Render a JS value as a PowDB SQL literal. Same accepted types as
 * {@link escapeLiteral}: `string`, `number`, `bigint`, `boolean`, `null`.
 *
 * - string → `'...'` with `\` doubled and `'` doubled
 * - number → decimal; rejects `NaN`/`±Infinity`
 * - bigint → decimal digits; rejects anything outside the signed 64-bit range
 * - boolean → `true` / `false`
 * - null → `null`
 */
export function escapeSqlLiteral(
  value: string | number | bigint | boolean | null
): string {
  if (value === null) return "null";

  const t = typeof value;

  if (t === "string") {
    // Backslash first: this lexer treats `\` as an escape inside strings, so a
    // lone backslash would otherwise swallow the quote doubling that follows.
    const escaped = (value as string)
      .replace(/\\/g, "\\\\")
      .replace(/'/g, "''");
    return `'${escaped}'`;
  }

  if (t === "number") {
    const n = value as number;
    if (!Number.isFinite(n)) {
      throw new TypeError(
        `escapeSqlLiteral: non-finite number ${String(n)} cannot be represented as a SQL literal`
      );
    }
    return String(n);
  }

  if (t === "bigint") {
    return renderBigint("escapeSqlLiteral", value as bigint);
  }

  if (t === "boolean") {
    return value ? "true" : "false";
  }

  throw new TypeError(`escapeSqlLiteral: unsupported type ${describe(value)}`);
}

/**
 * Render a SQL identifier (table, column, alias). The validated name comes back
 * double-quoted — `escapeSqlIdent("order")` gives `"order"` — and `TypeError`
 * is thrown on any invalid input. Pass the bare name: a value that already
 * carries quotes is rejected rather than quoted twice.
 *
 * The quoting is unconditional. PowDB's SQL lexer reads `"..."` as an
 * identifier and re-emits it as a backtick-quoted PowQL word, which bypasses
 * every keyword check downstream, so quoting is the only spelling that lets a
 * reserved word like `order` or `group` name a table. It does no case folding,
 * so quoting a name that needed no quoting changes nothing. Doing it always
 * means there is no second keyword list to keep in step with the engine.
 *
 * Quoted identifiers need a server on 0.23.0 or newer. This is for names, not
 * types: a column type in `CREATE TABLE` is a keyword, not an identifier, and
 * must not be passed through here.
 */
export function escapeSqlIdent(name: string): string {
  if (typeof name !== "string") {
    throw new TypeError(`escapeSqlIdent: expected string, got ${typeof name}`);
  }
  if (name.length === 0) {
    throw new TypeError("escapeSqlIdent: identifier must not be empty");
  }
  if (!IDENT_RE.test(name)) {
    const hint = name.includes('"')
      ? " (pass the bare name; escapeSqlIdent adds the quotes a reserved word needs)"
      : "";
    throw new TypeError(
      `escapeSqlIdent: invalid identifier ${JSON.stringify(name)} (must match /^[A-Za-z_][A-Za-z0-9_]*$/)${hint}`
    );
  }
  return `"${name}"`;
}

/** Wrapper marking a string as a SQL identifier for the `sql` tagged template. */
export class SqlIdent {
  constructor(public readonly name: string) {}
}

/** Factory for {@link SqlIdent}. */
export function sqlIdent(name: string): SqlIdent {
  return new SqlIdent(name);
}

/**
 * Tagged template for SQL composition. Interpolated values are escaped as SQL
 * literals; wrap one in `sqlIdent(...)` to interpolate an identifier.
 *
 *     const q = sql`SELECT name FROM ${sqlIdent(table)} WHERE name = ${userName}`;
 *     await client.querySql(q);
 *
 * Escaping, not binding: see the section note above, and prefer PowQL's `$N`
 * parameters when the input is untrusted.
 */
export function sql(
  strings: TemplateStringsArray,
  ...values: unknown[]
): string {
  let out = strings[0] ?? "";
  for (let i = 0; i < values.length; i++) {
    out += renderSqlInterpolation(values[i]);
    out += strings[i + 1] ?? "";
  }
  return out;
}

// ──────────────────────────────────────────────────────────
// internals
// ──────────────────────────────────────────────────────────

function renderSqlInterpolation(value: unknown): string {
  if (value instanceof SqlIdent) {
    return escapeSqlIdent(value.name);
  }
  if (value instanceof PowqlIdent) {
    throw new TypeError(
      "sql: use sqlIdent(...) for SQL identifiers, not ident(...)"
    );
  }
  return escapeSqlLiteral(value as string | number | bigint | boolean | null);
}

function renderInterpolation(value: unknown): string {
  if (value instanceof PowqlIdent) {
    return escapeIdent(value.name);
  }
  // `escapeLiteral` enforces the allowed set — for anything else it throws.
  return escapeLiteral(value as string | number | bigint | boolean | null);
}

function describe(value: unknown): string {
  if (value === undefined) return "undefined";
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  const t = typeof value;
  if (t === "object") return "object";
  return t;
}
