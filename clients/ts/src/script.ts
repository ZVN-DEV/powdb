/**
 * Statement-aware PowQL script splitting.
 *
 * Mirrors the server-side splitter (`powdb_query::lexer::split_statements`)
 * exactly, so a script file behaves the same whether it is fed to the CLI's
 * `--exec` / `.powql` path or to {@link Client.execScript} over the wire:
 *
 *   - `;` splits statements only at the top level.
 *   - `;` inside a `"..."` string literal never splits. A backslash inside a
 *     string consumes the next character unconditionally (mirroring the
 *     PowQL lexer), so `\"` and `\;` stay inside the string.
 *   - `#` starts a comment that runs to end-of-line; a `;` inside a comment
 *     never splits.
 *   - Segments are trimmed; empty segments (leading/doubled/trailing `;`,
 *     blank lines) are dropped.
 *   - Never errors: an unterminated string simply makes the rest of the
 *     input the final segment.
 */

/**
 * Split a PowQL script into individual statements.
 *
 * String-literal- and `#`-comment-aware `;` splitting with the exact
 * semantics of the CLI/server splitter (see module docs above).
 */
export function splitStatements(input: string): string[] {
  type State = "normal" | "in-string" | "in-comment";

  const out: string[] = [];
  let start = 0;
  let state: State = "normal";

  for (let i = 0; i < input.length; i++) {
    const c = input[i]!;
    switch (state) {
      case "normal":
        if (c === ";") {
          const seg = input.slice(start, i).trim();
          if (seg.length > 0) out.push(seg);
          start = i + 1;
        } else if (c === '"') {
          state = "in-string";
        } else if (c === "#") {
          state = "in-comment";
        }
        break;
      case "in-string":
        // Mirror the lexer: a backslash consumes the next char
        // unconditionally, so `\"` and `\;` stay inside the string.
        if (c === "\\") {
          i++;
        } else if (c === '"') {
          state = "normal";
        }
        break;
      case "in-comment":
        if (c === "\n") state = "normal";
        break;
    }
  }

  const seg = input.slice(start).trim();
  if (seg.length > 0) out.push(seg);
  return out;
}
