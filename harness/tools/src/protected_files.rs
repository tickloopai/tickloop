//! Keeps host-owned rollback storage outside the files exposed to the model.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use crate::file_port::*;

pub(crate) type ProtectedPaths = Arc<RwLock<Vec<PathBuf>>>;

pub(crate) struct ProtectedFiles {
    inner: Arc<dyn FileHost>,
    paths: ProtectedPaths,
}

impl ProtectedFiles {
    pub(crate) fn wrap(inner: Arc<dyn FileHost>, paths: ProtectedPaths) -> Arc<dyn FileHost> {
        Arc::new(Self { inner, paths })
    }

    fn is_protected(&self, path: &Path) -> bool {
        self.paths
            .read()
            .unwrap()
            .iter()
            .any(|protected| path.starts_with(protected))
    }

    fn local_root(&self, path: &FilePath) -> Option<PathBuf> {
        let capabilities = self.inner.capabilities();
        match path.root() {
            Some(name) => capabilities
                .root(name)
                .and_then(|root| root.local_path.clone()),
            None => capabilities
                .folder()
                .map_or_else(
                    || {
                        capabilities
                            .roots()
                            .iter()
                            .any(|root| root.local_path.is_some())
                    },
                    |folder| folder.local_path.is_some(),
                )
                .then(|| PathBuf::from("/")),
        }
    }

    fn check(&self, path: &FilePath) -> Result<(), ExecutionError> {
        if self
            .local_root(path)
            .is_some_and(|root| self.is_protected(&root.join(path.path())))
        {
            Err(ExecutionError::new(
                error_code::ACCESS_DENIED,
                "file history backups are private host storage",
            ))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl FileHost for ProtectedFiles {
    fn capabilities(&self) -> FileCapabilities {
        self.inner.capabilities()
    }

    async fn inspect(&self, query: FileQuery) -> Result<FileInspection, ExecutionError> {
        self.check(query.path())?;
        let mut query = query;
        if query.is_enumeration()
            && let Some(root) = self.local_root(query.path())
        {
            let searched = root.join(query.path().path());
            let excluded = self
                .paths
                .read()
                .unwrap()
                .iter()
                .filter(|path| path.starts_with(&searched))
                .filter_map(|path| {
                    let relative = path.strip_prefix(&root).ok()?;
                    Some(match query.path().root() {
                        Some(name) => FilePath::in_root(name.clone(), &relative.to_string_lossy()),
                        None => FilePath::outside_roots(&path.to_string_lossy()),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            query = query.excluding(excluded);
        }
        let answer = self.inner.inspect(query).await?;
        if let FileInspection::Resolved(resolved) = &answer {
            self.check(&resolved.path)?;
        }
        Ok(answer)
    }

    async fn prepare_write(
        &self,
        path: FilePath,
        content: Vec<u8>,
        expected: Option<FileVersion>,
    ) -> Result<FileWritePreparation, ExecutionError> {
        self.check(&path)?;
        self.inner.prepare_write(path, content, expected).await
    }

    async fn delete(&self, path: FilePath, expected: FileVersion) -> Result<(), FileWriteError> {
        self.check(&path).map_err(FileWriteError::NotWritten)?;
        self.inner.delete(path, expected).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_roots::LocalRoots;

    #[tokio::test]
    async fn list_limits_count_visible_entries_before_truncation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut excluded = Vec::new();
        for index in 0..105 {
            let path = root.join(format!("a-private-{index:03}"));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("secret"), "private").unwrap();
            excluded.push(path);
        }
        std::fs::write(root.join("z-visible"), "public").unwrap();
        let local = Arc::new(LocalRoots::new(root.to_str(), &[]));
        let name = local.capabilities().folder().unwrap().name.clone();
        let files = ProtectedFiles::wrap(local, Arc::new(RwLock::new(excluded)));
        let FileInspection::List(list) = files
            .inspect(FileQuery::List {
                path: FilePath::in_root(name, "").unwrap(),
                limit: 100,
            })
            .await
            .unwrap()
        else {
            panic!("expected listing");
        };
        assert_eq!(
            list.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["z-visible"]
        );
        assert!(!list.truncated);
    }
}
