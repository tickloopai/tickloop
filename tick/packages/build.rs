// Names the target triple this build is for, which picks a package's download from a Tick
// marketplace; Rust offers it only to build scripts.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-env=TICK_PACKAGES_TARGET={}",
        std::env::var("TARGET").expect("cargo sets it")
    );
}
