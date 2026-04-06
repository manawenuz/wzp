/* tslint:disable */
/* eslint-disable */

/**
 * Symmetric encryption session using ChaCha20-Poly1305.
 *
 * Mirrors `wzp-crypto::session::ChaChaSession` for WASM.  Nonce derivation
 * and key setup are identical so WASM and native peers interoperate.
 */
export class WzpCryptoSession {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Decrypt a media payload with AAD.
     *
     * Returns plaintext on success, or throws on auth failure.
     */
    decrypt(header_aad: Uint8Array, ciphertext: Uint8Array): Uint8Array;
    /**
     * Encrypt a media payload with AAD (typically the 12-byte MediaHeader).
     *
     * Returns `ciphertext || poly1305_tag` (plaintext.len() + 16 bytes).
     */
    encrypt(header_aad: Uint8Array, plaintext: Uint8Array): Uint8Array;
    /**
     * Create from a 32-byte shared secret (output of `WzpKeyExchange.derive_shared_secret`).
     */
    constructor(shared_secret: Uint8Array);
    /**
     * Current receive sequence number (for diagnostics / UI stats).
     */
    recv_seq(): number;
    /**
     * Current send sequence number (for diagnostics / UI stats).
     */
    send_seq(): number;
}

export class WzpFecDecoder {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Feed a received symbol.
     *
     * Returns the decoded block (concatenated original frames, unpadded) if
     * enough symbols have been received to recover the block, or `undefined`.
     */
    add_symbol(block_id: number, symbol_idx: number, _is_repair: boolean, data: Uint8Array): Uint8Array | undefined;
    /**
     * Create a new FEC decoder.
     *
     * * `block_size` — expected number of source symbols per block.
     * * `symbol_size` — padded byte size of each symbol (must match encoder).
     */
    constructor(block_size: number, symbol_size: number);
}

export class WzpFecEncoder {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Add a source symbol (audio frame).
     *
     * Returns encoded packets (all source + repair) when the block is complete,
     * or `undefined` if the block is still accumulating.
     *
     * Each returned packet carries the 3-byte header:
     *   `[block_id][symbol_idx][is_repair]` followed by `symbol_size` bytes.
     */
    add_symbol(data: Uint8Array): Uint8Array | undefined;
    /**
     * Force-flush the current (possibly partial) block.
     *
     * Returns all source + repair symbols with headers, or empty vec if no
     * symbols have been accumulated.
     */
    flush(): Uint8Array;
    /**
     * Create a new FEC encoder.
     *
     * * `block_size` — number of source symbols (audio frames) per FEC block.
     * * `symbol_size` — padded byte size of each symbol (default 256).
     */
    constructor(block_size: number, symbol_size: number);
}

/**
 * X25519 key exchange: generate ephemeral keypair and derive shared secret.
 *
 * Usage from JS:
 * ```js
 * const kx = new WzpKeyExchange();
 * const ourPub = kx.public_key();         // Uint8Array(32)
 * // ... send ourPub to peer, receive peerPub ...
 * const secret = kx.derive_shared_secret(peerPub); // Uint8Array(32)
 * const session = new WzpCryptoSession(secret);
 * ```
 */
export class WzpKeyExchange {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Derive a 32-byte session key from the peer's public key.
     *
     * Raw DH output is expanded via HKDF-SHA256 with info="warzone-session-key",
     * matching `wzp-crypto::handshake::WarzoneKeyExchange::derive_session`.
     */
    derive_shared_secret(peer_public: Uint8Array): Uint8Array;
    /**
     * Generate a new random X25519 keypair.
     */
    constructor();
    /**
     * Our public key (32 bytes).
     */
    public_key(): Uint8Array;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_wzpcryptosession_free: (a: number, b: number) => void;
    readonly __wbg_wzpfecdecoder_free: (a: number, b: number) => void;
    readonly __wbg_wzpfecencoder_free: (a: number, b: number) => void;
    readonly __wbg_wzpkeyexchange_free: (a: number, b: number) => void;
    readonly wzpcryptosession_decrypt: (a: number, b: number, c: number, d: number, e: number) => [number, number, number, number];
    readonly wzpcryptosession_encrypt: (a: number, b: number, c: number, d: number, e: number) => [number, number, number, number];
    readonly wzpcryptosession_new: (a: number, b: number) => [number, number, number];
    readonly wzpcryptosession_recv_seq: (a: number) => number;
    readonly wzpcryptosession_send_seq: (a: number) => number;
    readonly wzpfecdecoder_add_symbol: (a: number, b: number, c: number, d: number, e: number, f: number) => [number, number];
    readonly wzpfecdecoder_new: (a: number, b: number) => number;
    readonly wzpfecencoder_add_symbol: (a: number, b: number, c: number) => [number, number];
    readonly wzpfecencoder_flush: (a: number) => [number, number];
    readonly wzpfecencoder_new: (a: number, b: number) => number;
    readonly wzpkeyexchange_derive_shared_secret: (a: number, b: number, c: number) => [number, number, number, number];
    readonly wzpkeyexchange_new: () => number;
    readonly wzpkeyexchange_public_key: (a: number) => [number, number];
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __externref_table_dealloc: (a: number) => void;
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
