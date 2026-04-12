/* tslint:disable */
/* eslint-disable */
export const memory: WebAssembly.Memory;
export const __wbg_wzpcryptosession_free: (a: number, b: number) => void;
export const __wbg_wzpfecdecoder_free: (a: number, b: number) => void;
export const __wbg_wzpfecencoder_free: (a: number, b: number) => void;
export const __wbg_wzpkeyexchange_free: (a: number, b: number) => void;
export const wzpcryptosession_decrypt: (a: number, b: number, c: number, d: number, e: number) => [number, number, number, number];
export const wzpcryptosession_encrypt: (a: number, b: number, c: number, d: number, e: number) => [number, number, number, number];
export const wzpcryptosession_new: (a: number, b: number) => [number, number, number];
export const wzpcryptosession_recv_seq: (a: number) => number;
export const wzpcryptosession_send_seq: (a: number) => number;
export const wzpfecdecoder_add_symbol: (a: number, b: number, c: number, d: number, e: number, f: number) => [number, number];
export const wzpfecdecoder_new: (a: number, b: number) => number;
export const wzpfecencoder_add_symbol: (a: number, b: number, c: number) => [number, number];
export const wzpfecencoder_flush: (a: number) => [number, number];
export const wzpfecencoder_new: (a: number, b: number) => number;
export const wzpkeyexchange_derive_shared_secret: (a: number, b: number, c: number) => [number, number, number, number];
export const wzpkeyexchange_new: () => number;
export const wzpkeyexchange_public_key: (a: number) => [number, number];
export const __wbindgen_exn_store: (a: number) => void;
export const __externref_table_alloc: () => number;
export const __wbindgen_externrefs: WebAssembly.Table;
export const __wbindgen_malloc: (a: number, b: number) => number;
export const __externref_table_dealloc: (a: number) => void;
export const __wbindgen_free: (a: number, b: number, c: number) => void;
export const __wbindgen_start: () => void;
