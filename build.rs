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
        .file("src/native/test_support.cpp")
        .compile("amaterasu_rtx_hdr");
    println!("cargo:rustc-link-lib=dl");
}
