fn main() {
    let target = std::env::var("TARGET").expect("TARGET set by cargo for build scripts");
    println!("cargo:rustc-env=PYDL_BUILD_TARGET={target}");
}
