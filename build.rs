fn main() {
    // Locate system Exiv2. 0.27.6 on this machine; 0.28 changes Image::AutoPtr to
    // Image::UniquePtr, which shim.cc version-guards, so either works.
    let exiv2 = pkg_config::Config::new()
        .atleast_version("0.27")
        .probe("exiv2")
        .unwrap_or_else(|e| {
            panic!(
                "\n\nlibexiv2 not found via pkg-config: {e}\n\
                 Install it with:  sudo apt install libexiv2-dev\n\n"
            )
        });

    println!("cargo:warning=building against exiv2 {}", exiv2.version);

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    let mut build = cxx_build::bridge("src/meta/mod.rs");
    build
        .file("src/meta/shim.cc")
        .std("c++17")
        .include(&manifest)
        .flag_if_supported("-Wno-deprecated-declarations");

    for path in &exiv2.include_paths {
        build.include(path);
    }

    build.compile("npc_meta");

    println!("cargo:rerun-if-changed=src/meta/mod.rs");
    println!("cargo:rerun-if-changed=src/meta/shim.cc");
    println!("cargo:rerun-if-changed=src/meta/shim.h");
}
