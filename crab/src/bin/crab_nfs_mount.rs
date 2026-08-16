use std::io;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("failed to launch sibling crab binary: {error}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> io::Result<i32> {
    let crab = sibling_crab_path()?;
    let status = Command::new(crab)
        .args(std::env::args_os().skip(1))
        .status()?;
    Ok(status.code().unwrap_or(1))
}

fn sibling_crab_path() -> io::Result<PathBuf> {
    Ok(sibling_crab_path_from(std::env::current_exe()?))
}

fn sibling_crab_path_from(mut path: PathBuf) -> PathBuf {
    path.set_file_name(crab_binary_name());
    path
}

fn crab_binary_name() -> &'static str {
    if cfg!(windows) { "crab.exe" } else { "crab" }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{crab_binary_name, sibling_crab_path_from};

    #[test]
    fn sibling_path_uses_platform_crab_name() {
        let helper = PathBuf::from("bin").join("crab-nfs-mount.exe");
        let expected = PathBuf::from("bin").join(crab_binary_name());

        assert_eq!(sibling_crab_path_from(helper), expected);
    }
}
