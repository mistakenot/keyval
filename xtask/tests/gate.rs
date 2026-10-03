//! Black-box tests of `./dev gate` candidate mode (PLAN.md §11.10 step 4, P11).
#![cfg(test)]

mod gate {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    const TREE: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A fresh directory under cargo's per-target temp dir.
    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs `command` free of the caller's git environment. These tests may themselves run
    /// inside a git-isolated candidate, and must still build and see their own fixture
    /// repository.
    fn scrubbed(mut command: Command) -> Output {
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CEILING_DIRECTORIES",
        ] {
            command.env_remove(var);
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn describe(out: &Output) -> String {
        format!(
            "status={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    }

    #[test]
    fn a_candidate_gate_run_that_can_see_a_repository_is_refused() {
        // A candidate nested in a repository, but run without the isolation environment.
        let repo = scratch("gate-not-isolated");
        let mut init = Command::new("git");
        init.args(["init", "--quiet"]).current_dir(&repo);
        assert!(scrubbed(init).status.success());
        let candidate = repo.join(".local/candidate");
        fs::create_dir_all(&candidate).unwrap();
        let artifacts = repo.join("artifacts");

        let mut gate = Command::new(env!("CARGO_BIN_EXE_xtask"));
        gate.args(["--json", "gate", "fast", "--tree", TREE, "--artifacts"])
            .arg(&artifacts)
            .env("KEYVAL_ROOT", &candidate)
            .current_dir(&candidate);
        let out = scrubbed(gate);
        let context = describe(&out);
        assert_eq!(out.status.code(), Some(1), "{context}");
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["schema"], "keyval.dev.error/v1", "{context}");
        assert_eq!(report["error"]["kind"], "refused", "{context}");
        let message = report["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("git-isolated") && message.contains(".git"),
            "rule: candidate mode refuses when git can see a repository (§11.10 step 4, \
             P11); {context}"
        );
        assert!(
            !artifacts.exists(),
            "rule: a refused run writes no receipt, so nothing can claim to judge this tree; \
             {context}"
        );
    }
}
