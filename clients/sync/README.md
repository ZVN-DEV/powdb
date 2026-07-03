# @zvndev/powdb-sync

Experimental embedded-sync orchestration for PowDB.

This package is the experimental V1 control-loop boundary for the Turso-style
PowDB variant: local embedded reads, remote-primary writes, and sync-down
retained WAL chunks.

It composes two lower-level packages:

- `@zvndev/powdb-embedded` for local readonly queries,
- `@zvndev/powdb-client` for authenticated primary sync frames and writes.

The native retained-unit apply binding is exposed by
`@zvndev/powdb-embedded` as `Database.applyRetainedUnits(...)`. Pass that
method through the local adapter. The native binding accepts the same
`databaseId` forms as this package's `SyncIdentity`: a 32-character hex string
or a 16-byte `Uint8Array`. The constructor validates that the local and remote
adapters expose the required methods before any sync work starts.

```ts
import { PowDBSyncReplica } from "@zvndev/powdb-sync";
import { Client } from "@zvndev/powdb-client";
import { Database } from "@zvndev/powdb-embedded";

const local = Database.open("./replica");
const remote = await Client.connect({
  host: "127.0.0.1",
  port: 5433,
  user: "replica",
  password: process.env.POWDB_PASSWORD,
});

const replica = new PowDBSyncReplica({
  replicaId: "device-1",
  identity: {
    databaseId: "00112233445566778899aabbccddeeff",
    primaryGeneration: 1n,
    walFormatVersion: 1,
    catalogVersion: 5,
    segmentFormatVersion: 1,
  },
  local: {
    queryReadonly: (query) => local.queryReadonly(query),
    applyRetainedUnits: (request) => local.applyRetainedUnits(request),
  },
  remote,
});

await replica.syncNow();
const syncLoop = replica.startBackgroundSync({
  intervalMs: 5_000,
  onError: (err) => console.warn("sync lag/error", err.code, err.message),
});
const rows = await replica.queryReadonly("User { .id, .name }");
syncLoop.stop();
```

## V1 Contract

- `queryReadonly(...)` reads the embedded replica and can be stale.
- `syncNow()` pulls bounded retained-unit chunks from the primary, applies them
  locally, then acknowledges only after local apply succeeds.
- `startBackgroundSync(...)` schedules `syncNow()` without overlapping runs.
  Use it for online catch-up; keep explicit `syncNow()` for deterministic
  read-after-sync points.
- `write(...)` sends DML to the primary. It does not queue offline writes.
- DDL through `write(...)` is rejected with `ddl_not_supported`.
- If a remote write may have committed but the response was lost, `write(...)`
  throws `commit_outcome_unknown`. The package does not retry non-idempotent
  writes silently.
- `WriteResult.localVisible` is true only when a local read is guaranteed to
  include the committed remote write. If local retained-unit apply succeeds but
  primary acknowledgement fails, `localVisibility` is `applied_but_unacked` and
  the result includes the applied and remote LSNs.

## Status

This package is experimental and version-locked to PowDB `0.7.2`. Pin matching
`@zvndev/powdb-client`, `@zvndev/powdb-embedded`, and server versions while
dogfooding.

The control loop is unit-tested with structural adapters, including background
sync scheduling. `test:native` exercises `PowDBSyncReplica.write(...)` through
the real `@zvndev/powdb-embedded` local adapter with a deterministic fake
primary. `test:e2e` combines `powdb-cli sync-enable`, full backup,
`powdb-server`, `@zvndev/powdb-client`, `powdb-cli sync-bootstrap`, native local
readonly queries, and retained-unit apply in one flow. CI now runs the current
JS sync vertical slice. Public beta still needs broader stale/rebootstrap,
DDL-policy, crash/interruption, idempotency, metrics, and package release
coverage.

## Development

```bash
pnpm install
pnpm run build
pnpm test
# Requires the local embedded addon to be built first:
#   cd ../../bindings/node && npm run build
pnpm run test:native
pnpm run test:e2e
```

`test:native` and `test:e2e` resolve the embedded addon in this order:
`POWDB_SYNC_NATIVE_EMBEDDED_ENTRY`, installed `@zvndev/powdb-embedded`, then the
repo-local `bindings/node` build artifact. The repo-local fallback is used only
when the package is not installed; an installed package that fails to load still
fails the test.
