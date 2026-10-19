use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::Source;

/// A shared daemon may change versions only within its original installation source.
/// Two unregistered executables remain usable by development fixtures.
/// Both origins are identified from metadata without probing either executable.
pub fn ensure_same_runtime_source(
    running_executable: &Path,
    candidate_executable: &Path,
) -> Result<()> {
    let running = running_executable.canonicalize().with_context(|| {
        format!(
            "Cannot resolve current runtime executable {}",
            running_executable.display()
        )
    })?;
    let candidate = candidate_executable.canonicalize().with_context(|| {
        format!(
            "Cannot resolve target runtime executable {}",
            candidate_executable.display()
        )
    })?;
    let running_source = Source::detect(&running).with_context(|| {
        format!(
            "Cannot identify current runtime source at {}",
            running.display()
        )
    })?;
    let candidate_source = Source::detect(&candidate).with_context(|| {
        format!(
            "Cannot identify target runtime source at {}",
            candidate.display()
        )
    })?;
    if running_source != candidate_source {
        bail!(
            "Cannot switch the running daemon to another installation source.\nCurrent: {}; executable {}\nTarget: {}; executable {}",
            describe_source(running_source.as_ref()),
            running.display(),
            describe_source(candidate_source.as_ref()),
            candidate.display(),
        );
    }
    Ok(())
}

fn describe_source(source: Option<&Source>) -> String {
    let Some(source) = source else {
        return "unknown (unregistered executable)".into();
    };
    let details = match source {
        Source::Native { root } | Source::Checkout { root } => format!("root {}", root.display()),
        Source::Homebrew { executable } => format!("brew {}", executable.display()),
        Source::Cargo { root, repository } => {
            format!("root {}; repository {repository}", root.display())
        }
        Source::CargoRegistry { root, launcher } => {
            format!("root {}; launcher {}", root.display(), launcher.display())
        }
    };
    format!("{} ({details})", source.label())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use crate::{Build, Installation, Source, ensure_same_runtime_source};

    fn executable(directory: &Path, version: &str, source: Option<&Source>) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let executable = directory.join("tickloop");
        std::fs::write(&executable, "source-only fixture; must never execute").unwrap();
        if let Some(source) = source {
            Installation {
                source: source.clone(),
                build: Build {
                    version: version.into(),
                    identity: format!("{version}+fixture"),
                },
                pinned: true,
            }
            .write(directory)
            .unwrap();
        }
        executable.canonicalize().unwrap()
    }

    #[test]
    fn runtime_source_matrix_preserves_exact_origins_across_versions() {
        let directory = tempfile::tempdir().unwrap();
        let mut sources = vec![None];
        for root in [
            directory.path().join("first"),
            directory.path().join("second"),
        ] {
            sources.extend([
                Some(Source::Native { root: root.clone() }),
                Some(Source::Homebrew {
                    executable: root.join("bin/brew"),
                }),
                Some(Source::Cargo {
                    root: root.clone(),
                    repository: "https://example.test/original".into(),
                }),
                Some(Source::CargoRegistry {
                    root: root.clone(),
                    launcher: root.join("bin/tickloop"),
                }),
                Some(Source::Checkout { root }),
            ]);
        }
        sources.push(Some(Source::Cargo {
            root: directory.path().join("first"),
            repository: "https://example.test/another".into(),
        }));
        sources.push(Some(Source::CargoRegistry {
            root: directory.path().join("first"),
            launcher: directory.path().join("another-launcher/tickloop"),
        }));
        let runtimes = sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                (
                    executable(
                        &directory.path().join(format!("running-{index}")),
                        "1.0.0",
                        source.as_ref(),
                    ),
                    executable(
                        &directory.path().join(format!("candidate-{index}")),
                        "2.0.0",
                        source.as_ref(),
                    ),
                )
            })
            .collect::<Vec<_>>();
        for (running_index, running_source) in sources.iter().enumerate() {
            for (candidate_index, candidate_source) in sources.iter().enumerate() {
                let running = &runtimes[running_index].0;
                let candidate = &runtimes[candidate_index].1;
                let result = ensure_same_runtime_source(running, candidate);
                if running_source == candidate_source {
                    result.unwrap_or_else(|error| panic!("{running_source:?}: {error:#}"));
                } else {
                    let message = result.unwrap_err().to_string();
                    for expected in [
                        "Current:",
                        "Target:",
                        running.to_str().unwrap(),
                        candidate.to_str().unwrap(),
                        running_source.as_ref().map_or("unknown", Source::label),
                        candidate_source.as_ref().map_or("unknown", Source::label),
                    ] {
                        assert!(
                            message.contains(expected),
                            "{expected:?} missing from {message}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn runtime_source_resolves_executable_links_before_reading_registration() {
        let directory = tempfile::tempdir().unwrap();
        let source = Source::Native {
            root: directory.path().join("installation"),
        };
        let running = executable(&directory.path().join("running"), "1.0.0", Some(&source));
        let candidate = executable(&directory.path().join("candidate"), "2.0.0", Some(&source));
        let link = directory.path().join("tickloop");
        std::os::unix::fs::symlink(&candidate, &link).unwrap();
        ensure_same_runtime_source(&running, &link).unwrap();
    }

    fn probing_executable(path: &Path, marker: &Path) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!(
            "#!/bin/sh\n: > '{}'\ncase \"$1\" in\n--version) echo 'tickloop 2.0.0';;\n--build-identity) echo '2.0.0+fixture';;\n*) exit 2;;\nesac\n",
            marker.display(),
        )).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path.to_owned()
    }

    #[test]
    fn runtime_source_refuses_unrecorded_homebrew_without_probing_its_executable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let running = executable(
            &root.join("running"),
            "1.0.0",
            Some(&Source::Native {
                root: root.join("native"),
            }),
        );
        let prefix = root.join("brew/Cellar/tickloop/2.0.0");
        let marker = root.join("candidate-executed");
        let candidate = probing_executable(&prefix.join("bin/tickloop"), &marker);
        let brew = root.join("brew/bin/brew");
        std::fs::create_dir_all(brew.parent().unwrap()).unwrap();
        std::fs::write(&brew, "metadata-only Homebrew marker").unwrap();
        std::fs::write(
            prefix.join("INSTALL_RECEIPT.json"),
            serde_json::to_vec(&serde_json::json!({"source":{"tap":"tickloopai/tap"}})).unwrap(),
        )
        .unwrap();
        assert!(!prefix.join("bin/installation.json").exists());
        let error = ensure_same_runtime_source(&running, &candidate).unwrap_err();
        assert!(error.to_string().contains("Homebrew"), "{error:#}");
        assert!(
            !marker.exists(),
            "source refusal must not execute the candidate"
        );
        let installation = Installation::detect(&candidate).unwrap().unwrap();
        assert_eq!(installation.source, Source::Homebrew { executable: brew });
        assert_eq!(installation.build.version, "2.0.0");
        assert!(
            marker.exists(),
            "normal installation detection still probes its build"
        );
    }

    #[test]
    fn runtime_source_refuses_unrecorded_cargo_git_without_probing_its_executable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let running = executable(
            &root.join("running"),
            "1.0.0",
            Some(&Source::Native {
                root: root.join("native"),
            }),
        );
        let cargo = root.join("cargo");
        let marker = root.join("candidate-executed");
        let candidate = probing_executable(&cargo.join("bin/tickloop"), &marker);
        let repository = "https://example.test/original";
        let installs = [
            "tickloop-agent",
            "tickloop-tickhost",
            "tickloop-tick-reader",
        ]
        .into_iter()
        .map(|name| {
            (
                format!("{name} 2.0.0 (git+{repository}#{})", "a".repeat(40)),
                serde_json::json!({}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
        std::fs::write(
            cargo.join(".crates2.json"),
            serde_json::to_vec(&serde_json::json!({"installs":installs})).unwrap(),
        )
        .unwrap();
        assert!(!cargo.join("bin/installation.json").exists());
        let error = ensure_same_runtime_source(&running, &candidate).unwrap_err();
        assert!(error.to_string().contains("Cargo (Git)"), "{error:#}");
        assert!(
            !marker.exists(),
            "source refusal must not execute the candidate"
        );
        let installation = Installation::detect(&candidate).unwrap().unwrap();
        assert_eq!(
            installation.source,
            Source::Cargo {
                root: cargo,
                repository: repository.into()
            }
        );
        assert_eq!(installation.build.version, "2.0.0");
        assert!(
            marker.exists(),
            "normal installation detection still probes its build"
        );
    }
}
