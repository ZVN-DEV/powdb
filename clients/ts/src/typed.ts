/**
 * Optional row-level type coercion for `queryTyped`.
 *
 * The server's wire format serialises every value to a string (see
 * `crates/server/src/handler.rs::value_to_display`):
 *   Int      → decimal digits, e.g. "42" or "-7"
 *   Float    → Rust `{}` format, e.g. "3.14"
 *   Bool     → "true" / "false"
 *   Str      → raw UTF-8 (may contain anything)
 *   DateTime → microseconds since Unix epoch as decimal, e.g. "1718928000000000"
 *   Uuid     → canonical "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
 *   Bytes    → "<N bytes>" (LOSSY — see note below)
 *
 * `queryTyped` takes a user-supplied schema and coerces each column string
 * into the corresponding JS value. Rationale for requiring the caller to
 * supply the schema (rather than introspecting): PowDB has no `describe`
 * statement yet, and going via `select` off a system catalog is one extra
 * round trip per typed query. When schema introspection lands, a helper
 * that builds the schema object can be added without changing this API.
 *
 * Bytes caveat: the current wire format is lossy for byte columns (renders
 * `<N bytes>`). Until the wire protocol grows a binary column type, this
 * module throws on `bytes` columns rather than guessing — use `str` in the
 * schema if you just want the `<N bytes>` placeholder.
 */

import { PowDBError } from "./errors.js";

/**
 * Supported column types. Mirrors the server's `DataType` enum minus the
 * `Bytes` variant, which is intentionally unsupported here.
 */
export type ColumnType = "int" | "float" | "bool" | "str" | "datetime" | "uuid";

/** Map of column name → declared type. Columns not in the map pass through as strings. */
export type TypedSchema = Record<string, ColumnType>;

/** Coerced value type. `int` may return `bigint` if the value exceeds Number.MAX_SAFE_INTEGER. */
export type Coerced = number | bigint | boolean | string | Date | null;

/** A row of coerced values keyed by column name. */
export type TypedRow = Record<string, Coerced>;

/**
 * Coerce a single string value to the JS type declared in `schema`. `null`
 * is returned for the conventional null sentinel — see note at call site.
 * Unknown column types fall through as strings.
 */
export function coerceValue(
  column: string,
  raw: string,
  type: ColumnType | undefined,
): Coerced {
  // The server's wire format distinguishes NULL from empty string by the
  // literal "null" bareword in scalar displays. For typed columns we treat
  // the single token "null" as NULL; empty string remains empty string for
  // `str` columns (can be an intentional empty value).
  if (type !== "str" && raw === "null") return null;

  switch (type) {
    case undefined:
    case "str":
      return raw;

    case "int": {
      if (!/^-?\d+$/.test(raw)) {
        throw new PowDBError(
          `queryTyped: column "${column}" declared int but got ${JSON.stringify(raw)}`,
          "type_coercion_failed",
        );
      }
      // Promote to BigInt when the value doesn't round-trip through Number.
      const n = Number(raw);
      if (Number.isSafeInteger(n)) return n;
      return BigInt(raw);
    }

    case "float": {
      const n = Number(raw);
      if (!Number.isFinite(n)) {
        throw new PowDBError(
          `queryTyped: column "${column}" declared float but got ${JSON.stringify(raw)}`,
          "type_coercion_failed",
        );
      }
      return n;
    }

    case "bool":
      if (raw === "true") return true;
      if (raw === "false") return false;
      throw new PowDBError(
        `queryTyped: column "${column}" declared bool but got ${JSON.stringify(raw)}`,
        "type_coercion_failed",
      );

    case "datetime": {
      if (!/^-?\d+$/.test(raw)) {
        throw new PowDBError(
          `queryTyped: column "${column}" declared datetime but got ${JSON.stringify(raw)} (expected microseconds since epoch)`,
          "type_coercion_failed",
        );
      }
      // Server sends microseconds since epoch. JS Date uses milliseconds.
      // BigInt division avoids float precision loss for far-future dates.
      const micros = BigInt(raw);
      const millis = Number(micros / 1000n);
      return new Date(millis);
    }

    case "uuid": {
      // Canonical 8-4-4-4-12 hex, lowercase (per server format).
      if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(raw)) {
        throw new PowDBError(
          `queryTyped: column "${column}" declared uuid but got ${JSON.stringify(raw)}`,
          "type_coercion_failed",
        );
      }
      return raw;
    }
  }
}

/**
 * Coerce a single result row (a string tuple) into a typed object keyed by
 * column name.
 */
export function coerceRow(
  columns: string[],
  values: string[],
  schema: TypedSchema,
): TypedRow {
  const out: TypedRow = {};
  for (let i = 0; i < columns.length; i++) {
    const col = columns[i]!;
    out[col] = coerceValue(col, values[i] ?? "null", schema[col]);
  }
  return out;
}

/** Coerce every row in a rows-result into typed objects. */
export function coerceRows(
  columns: string[],
  rows: string[][],
  schema: TypedSchema,
): TypedRow[] {
  return rows.map((r) => coerceRow(columns, r, schema));
}
