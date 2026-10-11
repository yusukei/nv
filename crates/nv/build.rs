fn main() {
    // The target triple names the release asset to fetch on `nv self update`.
    println!(
        "cargo:rustc-env=NV_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
    // CI sets this to https://github.com/<owner>/<repo>/releases/latest/download.
    println!("cargo:rerun-if-env-changed=NV_RELEASE_BASE");
    if let Ok(base) = std::env::var("NV_RELEASE_BASE") {
        println!("cargo:rustc-env=NV_RELEASE_BASE={base}");
    }
}
