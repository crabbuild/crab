//! Resolution of the sibling `crab` executable used by coordinator clients.

use std::path::Path;

pub fn crab_binary_path() -> String {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_crab") {
        return path;
    }

    let Ok(executable) = std::env::current_exe() else {
        return "crab".to_owned();
    };
    let resolved = executable.canonicalize().unwrap_or(executable);
    if is_cargo_test_harness(&resolved) {
        return std::env::temp_dir()
            .join("crab-test-binary-unavailable")
            .to_string_lossy()
            .into_owned();
    }

    if resolved
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("git-remote-"))
        && let Some(parent) = resolved.parent()
    {
        let crab = parent.join("crab");
        if crab.exists() {
            return crab.to_string_lossy().into_owned();
        }
    }

    resolved.to_string_lossy().into_owned()
}

fn is_cargo_test_harness(path: &Path) -> bool {
    // Cargo owns the artifact directory layout; only the harness name matters.
    let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((_, suffix)) = stem.rsplit_once('-') else {
        return false;
    };

    suffix.len() >= 8 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_harness_detection_does_not_depend_on_artifact_directory() {
        for directory in [
            "target/debug/deps",
            "target/debug/build/crab-vfs/0123456789abcdef/out",
        ] {
            assert!(is_cargo_test_harness(
                &Path::new(directory).join("crab_vfs-0123456789abcdef")
            ));
        }
    }

    #[test]
    fn coordinator_binary_never_resolves_to_the_current_test_process() {
        let current = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_ne!(crab_binary_path(), current.to_string_lossy());
    }
}
