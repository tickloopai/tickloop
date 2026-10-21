mod bundle;

use std::path::Path;
use std::process::Command;

const RELEASES: &str = "https://github.com/tickloopai/homebrew-tap/releases/download";
const MAX_ARCHIVE_BYTES: &str = "268435456";
const MAX_CHECKSUM_BYTES: &str = "4096";
const CONNECT_TIMEOUT_SECONDS: &str = "15";
const DOWNLOAD_TIMEOUT_SECONDS: &str = "600";

fn main() {
    for file in ["build.rs", "bundle.rs", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={file}");
    }
    if let Err(error) = prepare() {
        panic!("Cannot prepare the complete Tickloop installation: {error}");
    }
}

fn prepare() -> Result<(), String> {
    // Linux releases are static musl programs, so glibc hosts install the same archive.
    let target = match std::env::var("TARGET")
        .map_err(|error| error.to_string())?
        .as_str()
    {
        "aarch64-apple-darwin" => "aarch64-apple-darwin",
        "x86_64-apple-darwin" => "x86_64-apple-darwin",
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => "x86_64-unknown-linux-musl",
        _ => return Err("Tickloop supports macOS and Linux on ARM64 and x86_64 only.".into()),
    };
    let version = std::env::var("CARGO_PKG_VERSION").map_err(|error| error.to_string())?;
    let directory = std::path::PathBuf::from(
        std::env::var_os("OUT_DIR").ok_or("Missing Cargo build directory")?,
    );
    let payload = directory.join(format!("{version}-{target}"));
    std::fs::create_dir_all(&payload).map_err(|error| error.to_string())?;
    let archive = payload.join("release.tar.gz");
    let checksum_file = payload.join("release.sha256");
    let validation = payload.join("verified");
    std::fs::create_dir_all(&validation).map_err(|error| error.to_string())?;
    if !archive.is_file() || !checksum_file.is_file() {
        println!(
            "cargo:warning=Downloading and verifying Tickloop {version} ({target}) during installation; first launch needs no download."
        );
        let base = format!("{RELEASES}/v{version}/tickloop-{target}.tar.gz");
        download(
            &format!("{base}.sha256"),
            &checksum_file,
            MAX_CHECKSUM_BYTES,
        )?;
        download(&base, &archive, MAX_ARCHIVE_BYTES)?;
    }
    let checksum = std::fs::read_to_string(&checksum_file).map_err(|error| error.to_string())?;
    if let Err(error) = bundle::unpack(&archive, &checksum, target, &validation) {
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_file(&checksum_file);
        return Err(error);
    }
    let source = format!(
        "pub const TARGET: &str = {target:?};\npub const ARCHIVE: &[u8] = include_bytes!({archive:?});\npub const CHECKSUM: &str = include_str!({checksum_file:?});\n"
    );
    std::fs::write(directory.join("payload.rs"), source).map_err(|error| error.to_string())
}

fn download(url: &str, destination: &Path, limit: &str) -> Result<(), String> {
    let partial = destination.with_extension("partial");
    let status = Command::new("/usr/bin/curl")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--location",
            "--show-error",
            "--progress-bar",
            "--connect-timeout",
            CONNECT_TIMEOUT_SECONDS,
            "--max-time",
            DOWNLOAD_TIMEOUT_SECONDS,
            "--max-filesize",
            limit,
        ])
        .arg(url)
        .arg("--output")
        .arg(&partial)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|error| error.to_string())?;
    if !status.success() {
        let _ = std::fs::remove_file(partial);
        return Err("Cannot download the official release; check your connection and rerun cargo install tickloop.".into());
    }
    std::fs::rename(partial, destination).map_err(|error| error.to_string())
}
