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

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_store_free: (a: number, b: number) => void;
    readonly store_fromBytes: (a: number, b: number) => [number, number, number];
    readonly store_query: (a: number, b: number, c: number) => [number, number];
    readonly store_schemaVersion: (a: number) => number;
    readonly rust_sqlite_wasm_abort: () => void;
    readonly rust_sqlite_wasm_assert_fail: (a: number, b: number, c: number, d: number) => void;
    readonly rust_sqlite_wasm_calloc: (a: number, b: number) => number;
    readonly rust_sqlite_wasm_free: (a: number) => void;
    readonly rust_sqlite_wasm_getentropy: (a: number, b: number) => number;
    readonly rust_sqlite_wasm_localtime: (a: number) => number;
    readonly rust_sqlite_wasm_malloc: (a: number) => number;
    readonly rust_sqlite_wasm_realloc: (a: number, b: number) => number;
    readonly sqlite3_os_end: () => number;
    readonly sqlite3_os_init: () => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
