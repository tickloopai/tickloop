//! Internal transport fixture executable; the shipped tickloop binary never reads these files.
use std::path::PathBuf;

pub fn executable() -> PathBuf {
    let directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("cli_fixture-")
                && entry.path().extension().is_none()
                && entry.path().is_file()
        })
        .max_by_key(|entry| entry.metadata().unwrap().modified().unwrap())
        .expect("build the transport fixture with cargo test --test cli_fixture --no-run")
        .path()
}
