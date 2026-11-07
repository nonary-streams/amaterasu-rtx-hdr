fn main() {
    println!("cargo:rerun-if-changed=src/native");
    println!("cargo:rerun-if-env-changed=AMATERASU_CUDA_INCLUDE_DIR");
    if std::env::var_os("CARGO_FEATURE_LINUX_NVIDIA").is_none() {
        return;
    }
    assert_eq!(
        std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("linux"),
        "linux-nvidia requires Linux"
    );
    assert_eq!(
        std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").as_deref(),
        Ok("64")
    );
    let mut build = cc::Build::new();
    for package in ["egl", "gl", "gbm", "libdrm"] {
        let library = pkg_config::Config::new()
            .probe(package)
            .unwrap_or_else(|_| panic!("linux-nvidia requires {package} development files"));
        build.includes(library.include_paths);
    }
    embed_ptx(
        "src/native/cuda/output.ptx",
        "rtx_output_ptx.h",
        "am_rtx_output_ptx",
    );
    let out_dir = std::env::var("OUT_DIR").expect("cargo provides OUT_DIR");
    build.include(&out_dir);
    let cuda = std::env::var_os("AMATERASU_CUDA_INCLUDE_DIR")
        .unwrap_or_else(|| "/opt/cuda/include".into());
    build
        .cpp(true)
        .std("c++17")
        .include(cuda)
        .include("../../sdk/include")
        .file("src/native/runtime.cpp")
        .file("src/native/surface.cpp")
        .file("src/native/effect.cpp")
        .file("src/native/timing.cpp")
        .file("src/native/cuda_output.cpp")
        .file("src/native/test_support.cpp")
        .compile("amaterasu_rtx_hdr");
    println!("cargo:rustc-link-lib=dl");
}

/// Embeds checked-in PTX (see scripts/generate-cuda-ptx.sh) as a C++ byte
/// array, so building needs no CUDA compiler; the driver JIT-compiles it.
fn embed_ptx(source: &str, header: &str, symbol: &str) {
    println!("cargo:rerun-if-changed={source}");
    let ptx = std::fs::read(source).unwrap_or_else(|error| panic!("{source}: {error}"));
    let mut text = format!("#pragma once\nstatic const unsigned char {symbol}[] = {{");
    for (index, byte) in ptx.iter().chain(std::iter::once(&0u8)).enumerate() {
        if index % 24 == 0 {
            text.push('\n');
        }
        text.push_str(&format!("{byte},"));
    }
    text.push_str("\n};\n");
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join(header);
    std::fs::write(out, text).expect("write embedded PTX");
}
