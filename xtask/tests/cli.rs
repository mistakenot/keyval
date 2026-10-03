//! Black-box tests of the `./dev` entry point: argument handling, exit codes and the
//! JSON error envelope (PLAN.md §11.13, O-6).
#![cfg(test)]

mod cli {
    use std::process::{Command, Output};

    fn dev(args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("cannot run xtask with args={args:?}: {err}"))
    }

    fn describe(args: &[&str], out: &Output) -> String {
        format!(
            "args={args:?} status={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    }

    #[test]
    fn no_command_is_a_usage_error() {
        let args: [&str; 0] = [];
        let out = dev(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "rule: ./dev with no command prints usage and exits 2; {}",
            describe(&args, &out)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Usage: ./dev"),
            "rule: usage text goes to stderr; {}",
            describe(&args, &out)
        );
    }

    #[test]
    fn unknown_command_is_a_usage_error() {
        let args = ["nope"];
        let out = dev(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "rule: an unknown command exits 2 and runs nothing; {}",
            describe(&args, &out)
        );
        assert!(
            out.stdout.is_empty(),
            "rule: human-mode errors go to stderr only; {}",
            describe(&args, &out)
        );
    }

    #[test]
    fn unknown_command_in_json_mode_prints_the_error_envelope() {
        let args = ["--json", "nope"];
        let out = dev(&args);
        assert_eq!(out.status.code(), Some(2), "{}", describe(&args, &out));
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
            panic!(
                "rule: robot mode prints exactly one JSON object; parse error={err}; {}",
                describe(&args, &out)
            )
        });
        assert_eq!(
            value["schema"],
            "keyval.dev.error/v1",
            "{}",
            describe(&args, &out)
        );
        assert_eq!(value["ok"], false, "{}", describe(&args, &out));
        assert_eq!(value["error"]["kind"], "usage", "{}", describe(&args, &out));
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("nope"),
            "rule: the message names the offending argument; message={message:?}; {}",
            describe(&args, &out)
        );
    }

    #[test]
    fn version_and_help_succeed() {
        for args in [["--version"], ["--help"]] {
            let out = dev(&args);
            assert_eq!(
                out.status.code(),
                Some(0),
                "rule: help and version requests exit 0; {}",
                describe(&args, &out)
            );
            assert!(!out.stdout.is_empty(), "{}", describe(&args, &out));
        }
    }
}
