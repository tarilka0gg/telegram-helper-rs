use std::{env, path::PathBuf, process::Command};

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../zig/src/native.zig");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed={}", root.display());
    let opt = if env::var("PROFILE").as_deref() == Ok("release") { "ReleaseFast" } else { "Debug" };
    let status = Command::new("zig")
        .args(["build-lib", "-static", "-O", opt, "-fPIC", "-lc", "-mcpu=baseline", "--name", "tghnative"])
        .arg(format!("-femit-bin={}", out.join("libtghnative.a").display()))
        .arg(&root)
        .current_dir(&out)
        .status()
        .expect("`zig` not found in PATH (install dev-lang/zig-bin)");
    assert!(status.success(), "zig build-lib failed");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=tghnative");
}
