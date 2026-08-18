//! Workflow stage command contracts.

use serde::{Deserialize, Serialize};

/// Command executed by a stage, in either argv or shell form.
///
/// The variants hash differently on purpose: `Argv(["a", "b"])` bypasses the
/// shell while `Shell("a b")` runs through a shell adapter in the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cmd {
    Argv(Vec<String>),
    Shell(String),
    ShellList(Vec<String>),
}

/// Native shell adapter used by every shell-form workflow command.
///
/// Shell syntax is intentionally not translated between platforms. Authors
/// who need a portable workflow should use [`Cmd::Argv`] or write commands in
/// the target platform's native shell language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlatformShell {
    pub(crate) family: &'static str,
    pub(crate) program: &'static str,
    pub(crate) prefix_args: &'static [&'static str],
}

impl PlatformShell {
    /// Return the complete argument vector for one shell script.
    #[must_use]
    pub(crate) fn args(self, script: &str) -> Vec<String> {
        self.prefix_args
            .iter()
            .map(|arg| (*arg).to_owned())
            .chain(std::iter::once(script.to_owned()))
            .collect()
    }
}

/// Return the one platform shell descriptor used for shell stages, shell
/// lists, hooks, and their stage-hash fingerprint.
#[must_use]
pub(crate) const fn platform_shell() -> PlatformShell {
    #[cfg(windows)]
    {
        PlatformShell {
            family: "cmd",
            program: "cmd.exe",
            prefix_args: &["/D", "/S", "/C"],
        }
    }

    #[cfg(not(windows))]
    {
        PlatformShell {
            family: "posix-sh",
            program: "/bin/sh",
            prefix_args: &["-c"],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_serde_preserves_distinct_command_forms() {
        let commands = [
            Cmd::Argv(vec!["python".to_owned(), "train.py".to_owned()]),
            Cmd::Shell("python train.py".to_owned()),
            Cmd::ShellList(vec![
                "python prep.py".to_owned(),
                "python train.py".to_owned(),
            ]),
        ];

        for command in commands {
            let encoded = serde_json::to_string(&command).unwrap();
            let decoded: Cmd = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, command);
        }
    }

    #[test]
    fn platform_shell_has_native_arguments() {
        let shell = platform_shell();
        #[cfg(windows)]
        assert_eq!(
            (shell.family, shell.program, shell.args("echo hi")),
            (
                "cmd",
                "cmd.exe",
                vec![
                    "/D".to_owned(),
                    "/S".to_owned(),
                    "/C".to_owned(),
                    "echo hi".to_owned()
                ]
            )
        );
        #[cfg(not(windows))]
        assert_eq!(
            (shell.family, shell.program, shell.args("echo hi")),
            (
                "posix-sh",
                "/bin/sh",
                vec!["-c".to_owned(), "echo hi".to_owned()]
            )
        );
    }
}
