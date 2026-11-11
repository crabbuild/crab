import { spawn } from "node:child_process";
import { access, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const desktop = fileURLToPath(new URL("../", import.meta.url));
const target = process.env.CARGO_TARGET_DIR;
if (!target || !path.isAbsolute(target)) throw new Error("Set an absolute, isolated CARGO_TARGET_DIR.");
await access(target, constants.W_OK);
const directory = await mkdtemp(path.join(target, "publication-probe-"));
const source = await readFile(path.join(desktop, "agent/src/repository/content/publication.rs"), "utf8");
const start = source.indexOf("pub(super) fn unchanged_file(");
const end = source.indexOf("\n}", start);
if (start < 0 || end < 0) throw new Error("Publication metadata guard was not found.");
const guard = source.slice(start, end + 2).replace("pub(super) fn", "fn");
// Compile the actual guard before the expensive desktop build. The fixture uses
// tempfile's Windows CREATE flags and real Git, without the database/SDK graph.
const code = [String.raw`
use std::{fs::{self, File, OpenOptions}, io::{self, Write}, path::{Path, PathBuf}, process::Command};

fn root(name: &str) -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let path = executable.parent().ok_or_else(|| io::Error::other("probe directory missing"))?.join(name);
    fs::create_dir(&path)?;
    path.canonicalize()
}

fn temporary(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)] {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x100); // FILE_ATTRIBUTE_TEMPORARY, as in tempfile::create_named.
    }
    options.open(path)
}

fn git(root: &Path, args: &[&str], index: Option<&Path>) -> io::Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.arg("--literal-pathspecs").args(args).current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", root.join("missing-global"));
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES"] {
        command.env_remove(name);
    }
    if let Some(index) = index { command.env("GIT_INDEX_FILE", index); }
    let output = command.output()?;
    if !output.status.success() { return Err(io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned())); }
    Ok(output.stdout)
}

#[test]
fn written_temporary_survives_publication_guard() -> io::Result<()> {
    let root = root("temporary")?;
    let path = root.join("owned");
    let file = temporary(&path)?;
    let mut writer = file.try_clone()?;
    writer.write_all(&vec![3; 1024 * 1024])?;
    writer.flush()?;
    drop(writer);
    file.sync_all()?;
    file.set_permissions(file.metadata()?.permissions())?;
    let opened = file.metadata()?;
    let named = fs::symlink_metadata(&path)?;
    println!("[DEBUG-publication] opened={opened:?} named={named:?} canonical={:?} path={path:?}", path.canonicalize()?);
    assert!(unchanged_file(&opened, &named), "owned temporary was rejected by the actual metadata guard");
    assert_eq!(path.canonicalize()?, path, "owned temporary path was rejected");
    Ok(())
}

#[test]
fn copied_private_index_is_visible_to_git() -> io::Result<()> {
    let root = root("index")?;
    git(&root, &["init", "--quiet"], None)?;
    fs::create_dir_all(root.join("models/deep"))?;
    let name = "models/deep/data[1].bin";
    fs::write(root.join(name), vec![3; 1024 * 1024])?;
    git(&root, &["-c", "core.autocrlf=false", "add", "--", name], None)?;
    let expected = git(&root, &["ls-files", "--stage", "-z", "--", name], None)?;
    let private = root.join("private-index");
    let file = temporary(&private)?;
    let mut writer = file.try_clone()?;
    io::copy(&mut File::open(root.join(".git/index"))?, &mut writer)?;
    writer.flush()?;
    let selected = git(&root, &["ls-files", "--stage", "-z", "--", name], Some(&private));
    println!("[DEBUG-publication] open-index opened={:?} named={:?} selected={selected:?}", file.metadata()?, fs::symlink_metadata(&private)?);
    drop(writer);
    drop(file);
    let closed = git(&root, &["ls-files", "--stage", "-z", "--", name], Some(&private))?;
    assert_eq!(closed, expected, "private index differs after closing writers");
    assert_eq!(selected?, expected, "private index differs while writers remain open");
    Ok(())
}

#[test]
fn unchanged_working_file_survives_captured_metadata() -> io::Result<()> {
    let root = root("working")?;
    let path = root.join("working");
    fs::write(&path, b"retain working changes")?;
    let captured = fs::symlink_metadata(&path)?;
    let opened = File::open(&path)?.metadata()?;
    println!("[DEBUG-publication] working captured={captured:?} opened={opened:?}");
    assert!(unchanged_file(&captured, &opened), "unchanged working file was rejected");
    Ok(())
}

#[test]
fn workspace_replacement_preserves_content_after_writer_close() -> io::Result<()> {
    let root = root("workspace")?;
    let path = root.join("workspace");
    fs::create_dir(&path)?;
    let mut file = temporary(&path.join("payload"))?;
    file.write_all(b"owned partial content")?;
    let moved = root.join("moved");
    let open = fs::rename(&path, &moved);
    println!("[DEBUG-publication] rename-with-writer={open:?}");
    drop(file);
    match open {
        Ok(()) => {},
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => fs::rename(&path, &moved)?,
        Err(error) => return Err(error),
    }
    fs::create_dir(&path)?;
    fs::write(path.join("foreign"), b"retain foreign content")?;
    assert_eq!(fs::read(moved.join("payload"))?, b"owned partial content");
    assert_eq!(fs::read(path.join("foreign"))?, b"retain foreign content");
    Ok(())
}
`, guard].join("\n");
const input = path.join(directory, "probe.rs");
const binary = path.join(directory, process.platform === "win32" ? "probe.exe" : "probe");
await writeFile(input, code);
async function run(command, args) {
  await new Promise((resolve, reject) => {
    const child = spawn(command, args, { cwd: directory, stdio: "inherit" });
    child.once("error", reject);
    child.once("exit", code => code === 0 ? resolve() : reject(new Error(`${command} exited with status ${code}; probe retained at ${directory}`)));
  });
}
await run("rustc", ["--edition=2024", "--test", input, "-o", binary]);
await run(binary, ["--test-threads=1", "--nocapture"]);
