// What this package exposes to a consumer that resolves it by name.
//
// `main` is loader.js, which wraps the generated binding so every error carries
// this package's `code` and the engine's `errorClass`. Without an `exports`
// map every file in the tarball is also reachable by subpath, including the
// generated `index.js` the loader exists to wrap: reaching for that one gets an
// unclassified addon while index.d.ts promises a classified one.
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const PACKAGE_DIR = resolve(HERE, "..");
const NAME = "@zvndev/powdb-embedded";

/**
 * Run `source` in a fresh process that resolves this package by its published
 * name, the way an installed consumer does. Resolution by name is the only
 * thing an `exports` map governs: a relative require from inside the package
 * bypasses it, so the addon's own `require("./powdb-embedded.*.node")` keeps
 * working whatever the map says.
 */
function runAsConsumer(source, filename = "probe.cjs") {
  const dir = mkdtempSync(join(tmpdir(), "powdb-embedded-consumer-"));
  try {
    mkdirSync(join(dir, "node_modules", "@zvndev"), { recursive: true });
    symlinkSync(PACKAGE_DIR, join(dir, "node_modules", NAME), "dir");
    const script = join(dir, filename);
    writeFileSync(script, source);
    return execFileSync(process.execPath, [script], { encoding: "utf8" }).trim();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/** A probe that reports what `entry` throws when a bad open is attempted. */
const openProbe = (entry) => `
const out = {};
try {
  const { Database } = require(${JSON.stringify(entry)});
  try {
    Database.open("/powdb-embedded-no-such-parent/probe");
  } catch (err) {
    out.code = err.code;
    out.errorClass = err.errorClass;
  }
} catch (err) {
  out.resolveError = err.code;
}
console.log(JSON.stringify(out));
`;

test("the package entry point classifies what it throws", () => {
  const seen = JSON.parse(runAsConsumer(openProbe(NAME)));
  assert.equal(seen.resolveError, undefined, `${NAME} did not resolve`);
  assert.equal(seen.code, "open_failed");
  // The whole reason loader.js is the entry point.
  assert.equal(seen.errorClass, 0);
});

test("the unwrapped generated binding is not reachable by subpath", () => {
  // Reaching it got an addon whose errors carry no errorClass at all, which is
  // exactly what index.d.ts promises of "every entry point".
  const seen = JSON.parse(runAsConsumer(openProbe(`${NAME}/index.js`)));
  assert.equal(
    seen.resolveError,
    "ERR_PACKAGE_PATH_NOT_EXPORTED",
    `${NAME}/index.js still resolves, and it answers ${JSON.stringify(seen)}`,
  );
});

test("package.json stays reachable", () => {
  // Conventional, and read by tooling that has nothing to do with the addon.
  const seen = JSON.parse(runAsConsumer(`
console.log(JSON.stringify(require(${JSON.stringify(`${NAME}/package.json`)}).name));
`));
  assert.equal(seen, NAME);
});

test("the entry point stays reachable under its own filename", () => {
  // Reachable before the map existed, and it resolves to the same wrapped
  // module `.` does, so restricting it would break a caller for nothing.
  const seen = JSON.parse(runAsConsumer(openProbe(`${NAME}/loader.js`)));
  assert.equal(seen.resolveError, undefined, `${NAME}/loader.js no longer resolves`);
  assert.equal(seen.errorClass, 0);
});

test("README.md's own import line works, run as ESM through the exports map", () => {
  // Not a paraphrase of the README: the line is lifted out of it and executed.
  // A CommonJS `require` of this package gets its names off the live object, but
  // an ESM `import { ... }` gets them from cjs-module-lexer, which reads the
  // source statically and cannot see through `module.exports = native`. The two
  // views can disagree, and the documented one is the one that was wrong.
  const readme = readFileSync(join(PACKAGE_DIR, "README.md"), "utf8");
  const line = readme.match(
    new RegExp(`^import \\{([^}]*)\\} from "${NAME}";?$`, "m"),
  );
  assert.ok(line, `README.md no longer opens with a named import of ${NAME}`);
  const names = line[1]
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean);
  assert.ok(names.length > 0, "the README's import binds no names");

  // The bare specifier, so this goes through the exports map's "." entry rather
  // than reaching loader.js by path.
  const probe = `${line[0]}
console.log(JSON.stringify({${names.map((n) => `${n}: typeof ${n}`).join(", ")}}));
`;
  const seen = JSON.parse(runAsConsumer(probe, "probe.mjs"));
  for (const name of names) {
    assert.equal(
      seen[name],
      "function",
      `README.md imports { ${name} } from ${NAME}, and ESM sees ${seen[name]}`,
    );
  }
});

test("the statically named re-exports are exactly the addon's own exports", () => {
  // The wrapping loop in loader.js is the source of truth and stays drift-proof:
  // it classifies whatever the addon exports. The explicit assignments beside it
  // exist only so a static reader can see those names, and a static list rots.
  // Both directions are checked: a class added to the addon and not to the list
  // fails here, and so does a name in the list the addon no longer exports.
  const loader = readFileSync(join(PACKAGE_DIR, "loader.js"), "utf8");
  const named = [
    ...loader.matchAll(/^module\.exports\.(\w+) = native\.\1;$/gm),
  ].map((m) => m[1]);

  // Read off a pristine copy of the generated binding in its own process: the
  // loader mutates the object it loads, this one included, so the set has to be
  // taken somewhere the loader has not run.
  const indexJs = join(PACKAGE_DIR, "index.js");
  const addon = JSON.parse(
    execFileSync(
      process.execPath,
      [
        "-e",
        `console.log(JSON.stringify(Object.keys(require(${JSON.stringify(indexJs)}))))`,
      ],
      { encoding: "utf8" },
    ),
  );

  assert.deepEqual(
    [...named].sort(),
    [...addon].sort(),
    "loader.js's statically named re-exports have drifted from what the addon exports",
  );
});
