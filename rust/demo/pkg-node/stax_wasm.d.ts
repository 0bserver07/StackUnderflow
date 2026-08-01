/* tslint:disable */
/* eslint-disable */

/**
 * A `store.db` living in the page's own memory.
 */
export class Store {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Take the bytes of a dropped `store.db` and open them read-only.
     *
     * The `Uint8Array` is copied into wasm linear memory once, here; the page
     * can drop its own reference afterwards.
     *
     * # Errors
     * When the bytes are not a SQLite database.
     */
    static fromBytes(bytes: Uint8Array): Store;
    /**
     * Run one request; returns `{"stdout": "…", "code": 0}` or `{"error": "…"}`.
     *
     * `stdout` is byte-for-byte what `stax memory … --json` writes, trailing
     * newline included — that identity is what `rust/wasm-differ.sh` checks.
     */
    query(request_json: string): string;
    /**
     * `PRAGMA user_version` — the schema the store was written at.
     */
    schemaVersion(): number;
}
