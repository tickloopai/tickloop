use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

pub(super) struct Output {
    directory: Option<PathBuf>,
    limit: usize,
    head: String,
    tail: String,
    file: Option<File>,
    path: Option<PathBuf>,
    items: usize,
}
impl Output {
    pub(super) fn new(directory: Option<PathBuf>, limit: usize) -> Self {
        Self {
            directory,
            limit,
            head: String::new(),
            tail: String::new(),
            file: None,
            path: None,
            items: 0,
        }
    }
    pub(super) fn push(&mut self, text: &str) -> io::Result<()> {
        let separator = if self.items == 0 { "" } else { "\n" };
        self.items += 1;
        if let Some(file) = &mut self.file {
            file.write_all(separator.as_bytes())?;
            file.write_all(text.as_bytes())?;
            self.tail.push_str(separator);
            self.tail.push_str(text);
            self.keep_tail();
            return Ok(());
        }
        if self
            .head
            .chars()
            .count()
            .saturating_add(separator.len())
            .saturating_add(text.chars().count())
            <= self.limit
        {
            self.head.push_str(separator);
            self.head.push_str(text);
            return Ok(());
        }
        let directory = self.directory.as_ref().ok_or_else(|| {
            io::Error::other("Output exceeds the limit and no Family output directory is available")
        })?;
        tickloop_harness_base::private_fs::verify_private_directory(
            directory,
            "Codemode output directory",
        )?;
        let path = directory.join(format!("codemode-{}.txt", tickloop_ids::generate()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(self.head.as_bytes())?;
        file.write_all(separator.as_bytes())?;
        file.write_all(text.as_bytes())?;
        self.head.push_str(separator);
        self.head.push_str(text);
        self.tail = self.head.clone();
        self.head = self.head.chars().take(self.limit / 2).collect();
        self.keep_tail();
        self.file = Some(file);
        self.path = Some(path);
        Ok(())
    }
    fn keep_tail(&mut self) {
        let keep = self.limit - self.limit / 2;
        let skip = self.tail.chars().count().saturating_sub(keep);
        let start = self
            .tail
            .char_indices()
            .nth(skip)
            .map_or(self.tail.len(), |(index, _)| index);
        self.tail.drain(..start);
    }
    pub(super) fn finish(self) -> (String, Option<PathBuf>) {
        if self.path.is_some() {
            (
                format!("{}\n… output omitted …\n{}", self.head, self.tail),
                self.path,
            )
        } else {
            (self.head, None)
        }
    }
}
