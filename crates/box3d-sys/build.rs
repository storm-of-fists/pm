//! Compiles the vendored Box3D (see vendor/box3d/VENDOR.md for the
//! pin) plus the `pmb3` shim in ONE cc pass. No bindgen: Rust only
//! ever sees the shim's primitive-typed functions, so there is no
//! struct layout to generate — and no libclang requirement on any
//! machine that builds pm (Linux, native Windows, windows-gnu cross).

fn main() {
    let mut build = cc::Build::new();
    for entry in std::fs::read_dir("vendor/box3d/src").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "c") {
            build.file(path);
        }
    }
    build
        .file("src/pmb3.c")
        .include("vendor/box3d/include")
        .include("vendor/box3d/src")
        .std("c17")
        // Box3D builds clean on its own -Wall; silence cross-compiler
        // noise so OUR warnings stay readable.
        .warnings(false)
        .compile("box3d");
    println!("cargo:rerun-if-changed=src/pmb3.c");
    println!("cargo:rerun-if-changed=vendor/box3d");
}
