//! Black-box tests of `./dev check` argument handling (PLAN.md §11.13, P10). The case
//! selection and counting logic is tested against a fixture crate in `src/check.rs`.
#![cfg(test)]

mod check {
    use std::path::Path;
    use std::process::{Command, Output};

    fn dev(args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            // Located at run time: a compile-time path outlives its checkout
            // (br-gate-shared-target-dir-dtqu).
            .env(
                "KEYVAL_ROOT",
                Path::new(&std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join(".."),
            )
            .output()
            .unwrap()
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
    fn an_unknown_check_id_fails() {
        let args = ["check", "NOPE"];
        let out = dev(&args);
        assert_eq!(
            out.status.code(),
            Some(1),
            "rule: `./dev check NOPE` fails (W01 acceptance, P10); {}",
            describe(&args, &out)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("UNKNOWN"),
            "{}",
            describe(&args, &out)
        );
    }

    #[test]
    fn an_unknown_check_id_in_json_mode_is_reported_as_unknown() {
        let args = ["--json", "check", "NOPE"];
        let out = dev(&args);
        let context = describe(&args, &out);
        assert_eq!(out.status.code(), Some(1), "{context}");
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["schema"], "keyval.dev.check/v2", "{context}");
        assert_eq!(report["ok"], false, "{context}");
        assert_eq!(report["checks"][0]["id"], "NOPE", "{context}");
        assert_eq!(report["checks"][0]["status"], "unknown", "{context}");
    }

    #[test]
    fn check_without_an_id_is_a_usage_error() {
        let args = ["check"];
        let out = dev(&args);
        assert_eq!(out.status.code(), Some(2), "{}", describe(&args, &out));
    }
}
