// An unoptimized test binary's __eh_frame outgrows the 16 MB offsets of Apple's compact unwind
// table and ld warns on every link; debug builds skip the table, which only speeds unwinding.
fn main() {
    if std::env::var("PROFILE").as_deref() == Ok("debug")
        && std::env::var("CARGO_CFG_TARGET_VENDOR").as_deref() == Ok("apple")
    {
        println!("cargo:rustc-link-arg=-Wl,-no_compact_unwind");
    }
}
