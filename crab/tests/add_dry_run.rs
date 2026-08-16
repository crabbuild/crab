use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(dir: &Path, program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));
    assert!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }

    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

#[test]
fn dry_run_does_not_mutate_staging_attributes_objects_or_index() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    run(&repo, "git", &["init", "-q"]);
    run(&repo, "git", &["config", "user.name", "Crab Test"]);
    run(
        &repo,
        "git",
        &["config", "user.email", "crab@example.invalid"],
    );
    std::fs::write(repo.join(".gitattributes"), "*.bin filter=crab -text\n").unwrap();
    std::fs::write(repo.join("large.bin"), vec![0x5a; 2 * 1024 * 1024]).unwrap();
    run(&repo, "git", &["add", ".gitattributes"]);
    let before = snapshot(&repo);

    let output = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(["add", "--dry-run", "*.bin"])
        .current_dir(&repo)
        .env("HOME", temp.path().join("home"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "crab add --dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(snapshot(&repo), before);
}
