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
 *   - Identifiers are validated against `^[A-Za-z_][A-Za-z0-9_]*$` and passed
 *     through unchanged. Anything else throws a `TypeError`.
 *
 * PowQL string-escape rules (verified against `crates/query/src/lexer.rs`):
 *   - Strings are delimited by `"`.
 *   - `\\` → `\`, `\"` → `"`, `\n` → newline, `\t` → tab.
 *   - For any other `\X`, the backslash is dropped and `X` is kept literally.
 *   - A bare `"` terminates the string, so `"` inside must be `\"` and any
 *     literal backslash must be `\\` (otherwise it would swallow the next char).
 */

const IDENT_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** Wrapper marking a string as an identifier (vs a literal) for `powql` tagged templates. */
export class PowqlIdent {
  constructor(public readonly name: string) {}
}

/** Factory for {@link PowqlIdent}. Prefer this over `new PowqlIdent(...)` at call sites. */
export function ident(name: string): PowqlIdent {
  return new PowqlIdent(name);
}

/**
 * Validate a PowQL identifier (table name, field name, alias). Returns the
 * identifier unchanged on success; throws `TypeError` on any invalid input
 * (non-string, empty, or containing characters outside `[A-Za-z_][A-Za-z0-9_]*`).
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
    throw new TypeError(
      `escapeIdent: invalid identifier ${JSON.stringify(name)} (must match /^[A-Za-z_][A-Za-z0-9_]*$/)`
    );
  }
  return name;
}

/**
 * Render a JS value as a PowQL literal. Supports `string`, `number`, `bigint`,
 * `boolean`, and `null`. Rejects `NaN`/`±Infinity`, `undefined`, symbols,
 * objects, and arrays with `TypeError`.
 *
 * - string → `"..."` with `\` and `"` backslash-escaped (C-style, per the
 *   PowQL lexer). Backslash must be escaped first to avoid double-processing.
 * - number → decimal; rejects non-finite
 * - bigint → decimal digits
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
    return (value as bigint).toString(10);
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
 * - bigint → decimal digits
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
    return (value as bigint).toString(10);
  }

  if (t === "boolean") {
    return value ? "true" : "false";
  }

  throw new TypeError(`escapeSqlLiteral: unsupported type ${describe(value)}`);
}

/**
 * Validate a SQL identifier (table, column, alias). Returns it unchanged on
 * success and throws `TypeError` otherwise.
 *
 * Identifiers are validated rather than quoted because PowDB's SQL lexer reads
 * `"..."` as a string literal, so there is no identifier-quoting syntax to fall
 * back on. Only `^[A-Za-z_][A-Za-z0-9_]*$` is accepted.
 */
export function escapeSqlIdent(name: string): string {
  if (typeof name !== "string") {
    throw new TypeError(`escapeSqlIdent: expected string, got ${typeof name}`);
  }
  if (name.length === 0) {
    throw new TypeError("escapeSqlIdent: identifier must not be empty");
  }
  if (!IDENT_RE.test(name)) {
    throw new TypeError(
      `escapeSqlIdent: invalid identifier ${JSON.stringify(name)} (must match /^[A-Za-z_][A-Za-z0-9_]*$/)`
    );
  }
  return name;
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
