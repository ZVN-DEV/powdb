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
 * PowQL string-escape rules (as documented in the sprint plan):
 *   - Strings are delimited by `"`.
 *   - A literal `"` inside a string is represented as `""` (doubled, SQL-style).
 *   - Backslashes have no special meaning and are passed through unchanged.
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
 * - string → `"..."` with internal `"` doubled (SQL-style)
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
    return `"${(value as string).replace(/"/g, '""')}"`;
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
// internals
// ──────────────────────────────────────────────────────────

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
