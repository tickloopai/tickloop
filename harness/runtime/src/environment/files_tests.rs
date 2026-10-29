use super::*;

fn name(name: &str) -> RootName {
    RootName::new(name).unwrap()
}

fn root(root_name: &str, display_path: &str, is_folder: bool) -> FileRoot {
    FileRoot {
        name: name(root_name),
        display_path: display_path.into(),
        access: FileAccess::ReadWrite,
        is_folder,
        local_path: None,
    }
}

fn capabilities() -> FileCapabilities {
    FileCapabilities::new(
        vec![
            root("project", "/work/project", true),
            root("vendor", "/work/project/vendor", false),
            root("notes", "/notes", false),
        ],
        FileAccess::None,
    )
    .unwrap()
}

fn below(root_name: &str, path: &str) -> FilePath {
    FilePath::in_root(name(root_name), path).unwrap()
}

#[test]
fn root_names_are_short_single_components() {
    assert_eq!(name("project-2").as_str(), "project-2");
    for invalid in ["", "a/b", ".", "..", "nul\0"] {
        let error = RootName::new(invalid).unwrap_err();
        assert_eq!(error.code, error_code::INVALID_INPUT, "{invalid:?}");
    }
    assert!(RootName::new("x".repeat(ROOT_NAME_MAX_BYTES)).is_ok());
    assert!(RootName::new("x".repeat(ROOT_NAME_MAX_BYTES + 1)).is_err());
}

#[test]
fn paths_in_a_root_are_normalized_and_cannot_leave_it() {
    assert_eq!(below("project", "./src//lib.rs").path(), "src/lib.rs");
    assert_eq!(
        below("project", "src/../docs/./plan.md").path(),
        "docs/plan.md"
    );
    assert_eq!(below("project", "").path(), "");
    assert_eq!(below("project", "src/..").path(), "");
    for escaping in ["..", "../other", "src/../../other"] {
        let error = FilePath::in_root(name("project"), escaping).unwrap_err();
        assert_eq!(error.code, error_code::OUTSIDE_ROOT, "{escaping}");
    }
    assert_eq!(
        FilePath::in_root(name("project"), "/etc/passwd")
            .unwrap_err()
            .code,
        error_code::INVALID_INPUT
    );
    assert_eq!(
        FilePath::outside_roots("/tmp/../etc/./hosts")
            .unwrap()
            .path(),
        "/etc/hosts"
    );
    assert_eq!(
        FilePath::outside_roots("/..").unwrap_err().code,
        error_code::OUTSIDE_ROOT
    );
}

#[test]
fn relative_paths_resolve_in_the_folder() {
    let capabilities = capabilities();
    assert_eq!(
        capabilities.resolve("src/main.rs").unwrap(),
        below("project", "src/main.rs")
    );
    assert_eq!(capabilities.resolve(".").unwrap(), below("project", ""));
    assert_eq!(
        capabilities.resolve("vendor/lib.rs").unwrap(),
        below("vendor", "lib.rs")
    );
    let without_folder =
        FileCapabilities::new(vec![root("notes", "/notes", false)], FileAccess::ReadWrite).unwrap();
    assert_eq!(
        without_folder.resolve("a.txt").unwrap_err().code,
        error_code::INVALID_INPUT
    );
}

#[test]
fn absolute_paths_resolve_in_the_root_with_the_longest_display_path() {
    let capabilities = capabilities();
    assert_eq!(
        capabilities.resolve("/work/project/README.md").unwrap(),
        below("project", "README.md")
    );
    assert_eq!(
        capabilities.resolve("/work/project/vendor/x/y.rs").unwrap(),
        below("vendor", "x/y.rs")
    );
    assert_eq!(
        capabilities.resolve("/work/project/vendor").unwrap(),
        below("vendor", "")
    );
    assert_eq!(
        capabilities.resolve("/notes/today.md").unwrap(),
        below("notes", "today.md")
    );
    assert_eq!(
        capabilities.resolve("/work/project-other/a.txt").unwrap(),
        FilePath::outside_roots("/work/project-other/a.txt").unwrap(),
        "a display path matches whole components only"
    );
}

#[test]
fn paths_outside_every_root_have_no_root() {
    let capabilities = capabilities();
    let outside = capabilities.resolve("/etc/hosts").unwrap();
    assert_eq!(outside.root(), None);
    assert_eq!(outside.path(), "/etc/hosts");
    assert_eq!(capabilities.access(&outside), FileAccess::None);

    let left_the_folder = capabilities.resolve("../elsewhere.txt").unwrap();
    assert_eq!(left_the_folder.root(), None);
    assert_eq!(left_the_folder.path(), "/work/elsewhere.txt");
    assert_eq!(
        capabilities
            .resolve("/work/project/vendor/../notes.txt")
            .unwrap(),
        below("project", "notes.txt")
    );
    assert_eq!(
        capabilities.resolve("../../../..").unwrap_err().code,
        error_code::OUTSIDE_ROOT
    );
}

#[test]
fn capabilities_reject_ambiguous_roots_and_show_display_paths() {
    let duplicate_name = FileCapabilities::new(
        vec![root("a", "/one", true), root("a", "/two", false)],
        FileAccess::None,
    );
    assert!(duplicate_name.is_err());
    let duplicate_display = FileCapabilities::new(
        vec![root("a", "/one", true), root("b", "/one", false)],
        FileAccess::None,
    );
    assert!(duplicate_display.is_err());
    let two_folders = FileCapabilities::new(
        vec![root("a", "/one", true), root("b", "/two", true)],
        FileAccess::None,
    );
    assert!(two_folders.is_err());
    for display_path in ["relative", "/trailing/", "/a/../b", "/a//b"] {
        assert!(
            FileCapabilities::new(vec![root("a", display_path, true)], FileAccess::None).is_err(),
            "{display_path}"
        );
    }

    let capabilities = capabilities();
    assert_eq!(
        capabilities.display(&below("project", "src/main.rs")),
        "/work/project/src/main.rs"
    );
    assert_eq!(capabilities.display(&below("notes", "")), "/notes");
    assert_eq!(
        capabilities.display(&FilePath::outside_roots("/etc/hosts").unwrap()),
        "/etc/hosts"
    );
    let at_slash = FileCapabilities::new(vec![root("disk", "/", true)], FileAccess::None).unwrap();
    assert_eq!(at_slash.display(&below("disk", "tmp/a")), "/tmp/a");
    assert_eq!(at_slash.resolve("/tmp/a").unwrap(), below("disk", "tmp/a"));
}
