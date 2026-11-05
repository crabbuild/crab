use std::{io, os::windows::process::CommandExt, path::PathBuf, process::Stdio};
use tokio::{io::AsyncBufReadExt, process::Command};

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().unwrap();
    let cwd = PathBuf::from(args.next().unwrap());
    let git = args.next().unwrap();
    let mut input = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    input.next_line().await?;
    tokio::spawn(async move { while let Ok(Some(_)) = input.next_line().await {} });
    for (step, arguments) in [
        (
            &"rev-parse",
            &["--literal-pathspecs", "rev-parse", "--show-toplevel"][..],
        ),
        (&"remote get-url", &["remote", "get-url", "origin"][..]),
        (
            &"config --get",
            &["config", "--get", "remote.origin.url"][..],
        ),
    ] {
        println!("starting {step}");
        let mut cmd = Command::new(&git);
        cmd.args(arguments).current_dir(&cwd).kill_on_drop(true);
        if step == &"rev-parse" || mode == "null-stdin" {
            cmd.stdin(Stdio::null());
        }
        if mode == "no-console" {
            cmd.as_std_mut().creation_flags(0x08000000);
        }
        let status = cmd.output().await?.status;
        println!("finished {step}: {:?}", status.code());
    }
    Ok(())
}
