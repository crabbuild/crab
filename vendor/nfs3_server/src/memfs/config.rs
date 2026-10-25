use super::DELIMITER;

#[derive(Debug, Clone)]
pub(super) struct MemFsConfigEntry {
    pub(super) parent: String,
    pub(super) name: String,
    pub(super) is_dir: bool,
    pub(super) content: Vec<u8>,
}

/// Initial configuration for the in-memory file system.
///
/// It allows to specify the initial files and directories in the file system.
#[derive(Default, Debug, Clone)]
pub struct MemFsConfig {
    pub(super) entries: Vec<MemFsConfigEntry>,
}

impl MemFsConfig {
    /// Adds a directory to the file system configuration.
    ///
    /// # Panics
    ///
    /// Panics if the path is empty.
    pub fn add_dir(&mut self, path: &str) {
        let name = path
            .split(DELIMITER)
            .next_back()
            .expect("dir path cannot be empty")
            .to_string();
        let path = path.trim_end_matches(&name);
        self.entries.push(MemFsConfigEntry {
            parent: path.to_string(),
            name,
            is_dir: true,
            content: Vec::new(),
        });
    }

    /// Adds a file to the file system configuration.
    ///
    /// # Panics
    ///
    /// Panics if the path is empty.
    pub fn add_file(&mut self, path: &str, content: impl Into<Vec<u8>>) {
        let name = path
            .split(DELIMITER)
            .next_back()
            .expect("file path cannot be empty")
            .to_string();
        let path = path.trim_end_matches(&name);

        self.entries.push(MemFsConfigEntry {
            parent: path.to_string(),
            name,
            is_dir: false,
            content: content.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_config() {
        let mut config = MemFsConfig::default();
        config.add_file("/a.txt", b"hello world\n");
        config.add_file("/b.txt", b"Greetings to xet data\n");
        config.add_dir("/another_dir");
        config.add_file("/another_dir/thisworks.txt", b"i hope\n");

        assert_eq!(config.entries.len(), 4);
        assert_eq!(config.entries[0].parent, "/");
        assert_eq!(config.entries[1].parent, "/");
        assert_eq!(config.entries[2].parent, "/");
        assert_eq!(config.entries[3].parent, "/another_dir/");
        assert_eq!(config.entries[0].name, "a.txt");
        assert_eq!(config.entries[1].name, "b.txt");
        assert_eq!(config.entries[2].name, "another_dir");
        assert_eq!(config.entries[3].name, "thisworks.txt");
    }
}
