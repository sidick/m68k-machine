//! Wires `link.ld` into the linker invocation.
//!
//! Copying the script into `OUT_DIR` and pointing the linker at it via
//! `rustc-link-search` + `rustc-link-arg=-Tlink.ld` (rather than an
//! absolute `-C link-arg=-T<path>` baked into `.cargo/config.toml`) means
//! this works regardless of the directory cargo is invoked from, including
//! the canonical `cargo build -p board-qemu-virt --target
//! aarch64-unknown-none` run from the workspace root.

use std::env;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let dest = out_dir.join("link.ld");
    std::fs::copy("link.ld", &dest).expect("copy link.ld into OUT_DIR");

    println!("cargo:rustc-link-search={}", out_dir.display());
    println!("cargo:rustc-link-arg=-Tlink.ld");
    println!("cargo:rerun-if-changed=link.ld");
}
