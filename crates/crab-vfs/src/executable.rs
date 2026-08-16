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
    if path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        != Some("deps")
    {
        return false;
    }

    let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((_, suffix)) = stem.rsplit_once('-') else {
        return false;
    };

    suffix.len() >= 8 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}
