mod content_identity;

mod release_identity {
    include!("../../code/cli/build.rs");

    pub fn emit() {
        main();
    }
}

fn main() {
    release_identity::emit();
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest.join("../..").canonicalize().unwrap();
    let settings = content_identity::build_settings();
    let identity = content_identity::fingerprint(&workspace, &manifest, &settings)
        .expect("cannot identify Tickhost production inputs");
    for input in identity.inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    std::fs::write(
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("content-identity"),
        identity.value,
    )
    .unwrap();
}
