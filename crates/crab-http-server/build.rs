use std::fmt::Write as _;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")?;
    let root = Path::new(&manifest).join("../../packages/repository/dist");
    println!("cargo:rerun-if-changed={}", root.display());
    if !root.join("index.html").is_file() {
        return Err("build the React application first: npm ci --prefix packages/repository && npm run build --prefix packages/repository".into());
    }
    let root = root.canonicalize()?;
    let mut pending = vec![root.clone()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err("frontend assets cannot contain symlinks".into());
            }
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    let mut generated = String::from("pub(crate) const ASSETS: &[(&str, &[u8])] = &[\n");
    for path in files {
        let relative = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        writeln!(
            generated,
            "({relative:?}, include_bytes!({:?})),",
            path.to_string_lossy()
        )?;
    }
    generated.push_str("];\n");
    std::fs::write(
        Path::new(&std::env::var("OUT_DIR")?).join("assets.rs"),
        generated,
    )?;
    Ok(())
}
