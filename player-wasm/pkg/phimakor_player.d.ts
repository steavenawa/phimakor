/* tslint:disable */
/* eslint-disable */

/**
 * JS → wasm:一帧快照字节(PROTOCOL.md 0x01),解析后入队。
 */
export function handle_frame(data: Uint8Array): void;

/**
 * JS → wasm:一帧纹理字节(PROTOCOL.md 0x00 / 0xFF),解析后入队。
 */
export function handle_texture(data: Uint8Array): void;

/**
 * wasm 入口:启动 WebGPU + core 冒烟 + rAF 渲染循环。
 */
export function start(): void;

/**
 * JS → wasm:进入流模式(不再画内嵌测试快照)。
 */
export function start_stream(): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly handle_frame: (a: number, b: number) => void;
    readonly handle_texture: (a: number, b: number) => void;
    readonly start: () => void;
    readonly start_stream: () => void;
    readonly wasm_bindgen_1c5e775b114c88e7___convert__closures_____invoke___wasm_bindgen_1c5e775b114c88e7___JsValue__core_9b3796e30d99ddb7___result__Result_____wasm_bindgen_1c5e775b114c88e7___JsError___true_: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen_1c5e775b114c88e7___convert__closures_____invoke___wasm_bindgen_1c5e775b114c88e7___sys__JsOption_wgpu_f64b262944ba5ed9___backend__webgpu__webgpu_sys__gen_GpuError__GpuError___core_9b3796e30d99ddb7___result__Result_____wasm_bindgen_1c5e775b114c88e7___JsError___true_: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen_1c5e775b114c88e7___convert__closures_____invoke___wasm_bindgen_1c5e775b114c88e7___sys__JsOption_wgpu_f64b262944ba5ed9___backend__webgpu__webgpu_sys__gen_GpuError__GpuError___core_9b3796e30d99ddb7___result__Result_____wasm_bindgen_1c5e775b114c88e7___JsError___true__2: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen_1c5e775b114c88e7___convert__closures_____invoke___wasm_bindgen_1c5e775b114c88e7___sys__JsOption_wgpu_f64b262944ba5ed9___backend__webgpu__webgpu_sys__gen_GpuError__GpuError___core_9b3796e30d99ddb7___result__Result_____wasm_bindgen_1c5e775b114c88e7___JsError___true__3: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen_1c5e775b114c88e7___convert__closures_____invoke_______true_: (a: number, b: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_destroy_closure: (a: number, b: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
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
