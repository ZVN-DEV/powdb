/**
 * A tiny connection pool for the PowDB TypeScript client.
 *
 *     const pool = new Pool({ host, port, max: 8 });
 *
 *     // manual acquire/release
 *     const c = await pool.acquire();
 *     try {
 *       await c.query("...");
 *       pool.release(c);
 *     } catch (err) {
 *       // a broken client should NOT be returned to the pool
 *       pool.destroy(c);
 *       throw err;
 *     }
 *
 *     // ...or, preferred, use the scoped helper:
 *     const users = await pool.withClient((c) => c.query("Users"));
 *
 *     await pool.close();
 */

import {
  Client,
  type ClientOptions,
  type QueryResult,
  type ScriptStatementOutcome,
} from "./index.js";
import { PowDBError, isPowDBError } from "./errors.js";

export interface PoolOptions extends ClientOptions {
  /** Maximum concurrent clients. Default 10. */
  max?: number;
  /** Acquire timeout in ms. Default 30_000. */
  acquireTimeoutMs?: number;
  /**
   * How many times to retry `Client.connect` on transient errors
   * (connect_failed / timeout). Auth failures and other non-transient
   * errors are never retried. Default 3. Set to 0 to disable.
   */
  connectRetries?: number;
  /**
   * Initial backoff (ms) between connect retries. Each subsequent retry
   * doubles the delay up to `connectMaxBackoffMs`. Default 100.
   */
  connectBackoffMs?: number;
  /** Maximum backoff (ms) between connect retries. Default 2_000. */
  connectMaxBackoffMs?: number;
}

/**
 * Is this error worth retrying? Only the transient connect-phase errors
 * are retryable. Auth failures, size violations, aborts, and protocol
 * errors are not — retrying would just fail the same way.
 */
function isRetryable(err: unknown): boolean {
  if (!isPowDBError(err)) {
    // Raw socket errors surface as generic Error before we wrap them in
    // connect_failed. In practice connect() wraps them, but be defensive.
    return err instanceof Error;
  }
  return err.code === "connect_failed" || err.code === "timeout";
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    const t = setTimeout(resolve, ms);
    if (typeof t.unref === "function") t.unref();
  });
}

/**
 * Connect to the server with bounded retry + exponential backoff. Throws
 * the last error if every attempt fails, or the first error unchanged if
 * it was not retryable.
 */
async function connectWithRetry(
  opts: ClientOptions,
  retries: number,
  initialBackoffMs: number,
  maxBackoffMs: number,
): Promise<Client> {
  let lastErr: unknown;
  let delay = initialBackoffMs;
  for (let attempt = 0; attempt <= retries; attempt++) {
    try {
      return await Client.connect(opts);
    } catch (err) {
      lastErr = err;
      if (!isRetryable(err) || attempt === retries) break;
      await sleep(delay);
      delay = Math.min(delay * 2, maxBackoffMs);
    }
  }
  throw lastErr;
}

type Waiter = {
  resolve: (c: Client) => void;
  reject: (err: Error) => void;
  timer: NodeJS.Timeout | null;
};

/**
 * Lazy, FIFO connection pool.
 *
 * Semantics:
 *   - `acquire` returns an idle client if one exists; otherwise creates a new
 *     one up to `max`; otherwise waits until `release`/`destroy` frees a slot
 *     (subject to `acquireTimeoutMs`).
 *   - `release(c)` puts `c` back in the idle queue unconditionally. **If the
 *     caller observed a socket/connection error on `c`, they MUST call
 *     `destroy(c)` instead** — the pool cannot safely detect dead sockets.
 *   - `destroy(c)` closes `c` and decrements the live count, freeing a slot
 *     for a new client to be created.
 *   - `withClient(fn)` acquires, runs `fn`, and always destroys on error or
 *     releases on success. This is the safe default for most call sites.
 *   - `close()` rejects any pending waiters and awaits `close()` on every
 *     idle client. Subsequent calls to `acquire` reject immediately.
 */
export class Pool {
  private readonly opts: ClientOptions;
  private readonly max: number;
  private readonly acquireTimeoutMs: number;
  private readonly connectRetries: number;
  private readonly connectBackoffMs: number;
  private readonly connectMaxBackoffMs: number;

  private readonly idleClients: Client[] = [];
  private readonly waiters: Waiter[] = [];
  /** Clients owned by this pool (idle + checked-out). Used to reject
   *  foreign/double-destroyed clients from corrupting the slot accounting. */
  private readonly owned: Set<Client> = new Set();
  private live = 0;
  private _closed = false;

  constructor(opts: PoolOptions) {
    const {
      max = 10,
      acquireTimeoutMs = 30_000,
      connectRetries = 3,
      connectBackoffMs = 100,
      connectMaxBackoffMs = 2_000,
      ...clientOpts
    } = opts;
    if (!Number.isFinite(max) || max < 1) {
      throw new TypeError(`Pool: max must be a positive integer, got ${max}`);
    }
    if (!Number.isFinite(acquireTimeoutMs) || acquireTimeoutMs < 0) {
      throw new TypeError(
        `Pool: acquireTimeoutMs must be a non-negative number, got ${acquireTimeoutMs}`
      );
    }
    if (!Number.isFinite(connectRetries) || connectRetries < 0) {
      throw new TypeError(
        `Pool: connectRetries must be a non-negative integer, got ${connectRetries}`
      );
    }
    this.opts = clientOpts;
    this.max = max;
    this.acquireTimeoutMs = acquireTimeoutMs;
    this.connectRetries = connectRetries;
    this.connectBackoffMs = connectBackoffMs;
    this.connectMaxBackoffMs = connectMaxBackoffMs;
  }

  /**
   * Wraps Client.connect with the pool's configured retry policy. Used by
   * `acquire` and `drainWaiters`.
   */
  private connect(): Promise<Client> {
    return connectWithRetry(
      this.opts,
      this.connectRetries,
      this.connectBackoffMs,
      this.connectMaxBackoffMs,
    );
  }

  /** Live client count (idle + checked out). */
  get size(): number {
    return this.live;
  }

  /** Clients currently sitting in the idle queue. */
  get idle(): number {
    return this.idleClients.length;
  }

  /** `true` once {@link close} has been called. */
  get closed(): boolean {
    return this._closed;
  }

  /**
   * Acquire a client from the pool.
   *
   * Resolves with an idle client if one is available, creates a new one if
   * `live < max`, or waits in a FIFO queue. Rejects if `acquireTimeoutMs`
   * elapses, or if the pool is closed.
   */
  async acquire(): Promise<Client> {
    if (this._closed) {
      throw new Error("pool closed");
    }

    const existing = this.idleClients.shift();
    if (existing !== undefined) {
      return existing;
    }

    if (this.live < this.max) {
      this.live++;
      try {
        const c = await this.connect();
        this.owned.add(c);
        return c;
      } catch (err) {
        // Creation failed — we never had a live client, so give the slot
        // back and propagate.
        this.live--;
        // A newly-freed slot may let a pending waiter try again... but only
        // via a future release/destroy. To be fair to waiters, schedule a
        // drain here.
        this.drainWaiters();
        throw err;
      }
    }

    return new Promise<Client>((resolve, reject) => {
      const waiter: Waiter = {
        resolve,
        reject,
        timer: null,
      };
      if (this.acquireTimeoutMs > 0) {
        waiter.timer = setTimeout(() => {
          const idx = this.waiters.indexOf(waiter);
          if (idx !== -1) this.waiters.splice(idx, 1);
          reject(new Error("pool acquire timeout"));
        }, this.acquireTimeoutMs);
        // Don't keep the event loop alive just for the timeout.
        if (typeof waiter.timer.unref === "function") waiter.timer.unref();
      }
      this.waiters.push(waiter);
    });
  }

  /**
   * Return `c` to the idle queue so subsequent `acquire` calls can hand it
   * out. If the pool is closed, `c` is closed instead.
   *
   * **Do not call this if `c` has experienced a connection error** — use
   * {@link destroy} for that case.
   */
  release(c: Client): void {
    if (!this.owned.has(c)) {
      // Foreign client or already-destroyed — refuse to touch accounting.
      return;
    }

    if (this._closed) {
      // Pool is gone — just close the client, fire-and-forget.
      this.owned.delete(c);
      if (this.live > 0) this.live--;
      void c.close().catch(() => {});
      return;
    }

    const waiter = this.waiters.shift();
    if (waiter !== undefined) {
      if (waiter.timer !== null) clearTimeout(waiter.timer);
      waiter.resolve(c);
      return;
    }

    this.idleClients.push(c);
  }

  /**
   * Close `c` and free its slot so a new client can be created on the next
   * `acquire`. Use this when `c` has hit a socket error or is otherwise
   * known-bad.
   */
  destroy(c: Client): void {
    if (!this.owned.has(c)) {
      // Foreign client or already destroyed — don't touch slot accounting.
      return;
    }
    this.owned.delete(c);
    // Decrement first so any waiter we hand off to can create a fresh one.
    if (this.live > 0) this.live--;
    void c.close().catch(() => {});

    if (this._closed) return;

    // Drain one waiter if possible — the freed slot lets them create a new
    // connection.
    this.drainWaiters();
  }

  /**
   * Acquire a client, run `fn`, and either release (on success) or destroy
   * (on error). This is the recommended call site — it makes it impossible
   * to leak a client on an unhandled exception.
   */
  async withClient<T>(fn: (c: Client) => Promise<T>): Promise<T> {
    const c = await this.acquire();
    try {
      const out = await fn(c);
      this.release(c);
      return out;
    } catch (err) {
      this.destroy(c);
      throw err;
    }
  }

  /**
   * Run a multi-statement PowQL script on ONE pooled connection, pipelined.
   *
   * Checks out a single client for the whole script (statement order —
   * and `transactional: true` — therefore holds), delegates to
   * {@link Client.execScript}, and releases the client on success or
   * destroys it on error — same lifecycle as {@link withClient}.
   */
  async execScript(
    script: string,
    opts?: {
      continueOnError?: false;
      transactional?: boolean;
      signal?: AbortSignal;
    },
  ): Promise<QueryResult[]>;
  async execScript(
    script: string,
    opts: { continueOnError: true; signal?: AbortSignal },
  ): Promise<ScriptStatementOutcome[]>;
  async execScript(
    script: string,
    opts?: {
      continueOnError?: boolean;
      transactional?: boolean;
      signal?: AbortSignal;
    },
  ): Promise<QueryResult[] | ScriptStatementOutcome[]> {
    if (opts?.continueOnError === true) {
      return this.withClient((c) =>
        c.execScript(script, {
          continueOnError: true,
          ...(opts.transactional !== undefined
            ? { transactional: opts.transactional }
            : {}),
          ...(opts.signal ? { signal: opts.signal } : {}),
        } as { continueOnError: true; signal?: AbortSignal }),
      );
    }
    return this.withClient((c) =>
      c.execScript(script, {
        ...(opts?.transactional !== undefined
          ? { transactional: opts.transactional }
          : {}),
        ...(opts?.signal ? { signal: opts.signal } : {}),
      }),
    );
  }

  /**
   * Close the pool. Rejects all pending waiters, awaits `close()` on every
   * idle client, and marks the pool closed. Subsequent `acquire` calls
   * reject immediately.
   *
   * Checked-out clients are NOT tracked — callers that still hold one when
   * `close()` is called are responsible for closing it themselves.
   */
  async close(): Promise<void> {
    if (this._closed) return;
    this._closed = true;

    // Reject every pending waiter.
    while (this.waiters.length > 0) {
      const w = this.waiters.shift()!;
      if (w.timer !== null) clearTimeout(w.timer);
      w.reject(new Error("pool closed"));
    }

    // Close every idle client.
    const idle = this.idleClients.splice(0, this.idleClients.length);
    for (const c of idle) this.owned.delete(c);
    this.live -= idle.length;
    await Promise.all(idle.map((c) => c.close().catch(() => {})));
  }

  // ──────────────────────────────────────────────────────────
  // internals
  // ──────────────────────────────────────────────────────────

  private drainWaiters(): void {
    // If we have headroom AND a waiter, try to create a client for them.
    // (Handles the "acquire failed → slot freed" path cleanly without
    // forcing the caller to re-poll.)
    if (this._closed) return;
    while (this.waiters.length > 0 && this.live < this.max) {
      const waiter = this.waiters.shift()!;
      if (waiter.timer !== null) clearTimeout(waiter.timer);
      this.live++;
      this.connect().then(
        (c) => {
          // Pool may have been closed while the connect was in flight.
          // If so, close the freshly-opened client instead of handing it
          // to a waiter that's already been rejected.
          if (this._closed) {
            if (this.live > 0) this.live--;
            void c.close().catch(() => {});
            return;
          }
          this.owned.add(c);
          waiter.resolve(c);
        },
        (err) => {
          this.live--;
          waiter.reject(err as Error);
          // Keep trying for the next waiter, if any.
          this.drainWaiters();
        }
      );
    }
  }
}
