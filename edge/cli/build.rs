use std::env;

fn main() {
  println!("cargo:rustc-env=TARGET={}", env::var("TARGET").unwrap());
  println!("cargo:rustc-env=PROFILE={}", env::var("PROFILE").unwrap());

  // Export the N-API (and WebGPU) symbols in the `flow` binary's dynamic
  // symbol table so native Node addons (e.g. sharp) can resolve `napi_*`
  // symbols when they're dlopen'd. The `deno` binary does this in
  // cli/build.rs; the `flow` binary needs the same or addons fail to load
  // with `undefined symbol: napi_create_function`.
  deno_napi::print_linker_flags("flow");
  deno_webgpu::print_linker_flags("flow");
}
