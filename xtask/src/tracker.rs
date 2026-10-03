//! A typed port to the beads tracker (`br`, §11.1), for acceptance and closure. Output is
//! read through `--json` only (§11.5 rule 14). Tests substitute a fake.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::process;

const BR_LIMIT: Duration = Duration::from_mins(2);

/// What acceptance and closure need to know about a bead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bead {
    /// Tracker ID.
    pub id: String,
    /// `open`, `in_progress` or `closed`.
    pub status: String,
    /// Labels, e.g. `plan:W07`, `sub:W07`, `accept:DEV-COMMIT`.
    pub labels: Vec<String>,
}

/// Tracker operations.
pub trait Tracker {
    /// One bead.
    fn bead(&self, id: &str) -> Result<Bead, String>;
    /// Every bead, closed ones included.
    fn all_beads(&self) -> Result<Vec<Bead>, String>;
    /// Closes the bead with `reason`.
    fn close(&self, id: &str, reason: &str) -> Result<(), String>;
    /// Labels the bead `failed` + `needs-owner`, sets it `open` and records `comment`.
    fn mark_failed(&self, id: &str, comment: &str) -> Result<(), String>;
    /// Exports the tracker database to `.beads/issues.jsonl` (`br sync --flush-only`).
    fn flush(&self) -> Result<(), String>;
}

/// The real tracker: `br` run in the workspace root.
pub struct LiveTracker {
    /// The workspace root.
    pub root: PathBuf,
}

impl LiveTracker {
    fn br(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = Command::new("br");
        cmd.args(args).current_dir(&self.root);
        let captured = process::run(&mut cmd, BR_LIMIT)
            .map_err(|err| format!("br {}: {err}", args.join(" ")))?;
        if captured.status.success() {
            Ok(captured.stdout)
        } else {
            Err(format!(
                "br {} exited with {}: {}",
                args.join(" "),
                captured.status,
                captured.stderr.trim()
            ))
        }
    }
}

fn bead_from_json(value: &serde_json::Value) -> Option<Bead> {
    Some(Bead {
        id: value["id"].as_str()?.to_owned(),
        status: value["status"].as_str()?.to_owned(),
        labels: value["labels"]
            .as_array()
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| l.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

impl Tracker for LiveTracker {
    fn bead(&self, id: &str) -> Result<Bead, String> {
        let out = self.br(&["show", id, "--json"])?;
        let value: serde_json::Value =
            serde_json::from_str(&out).map_err(|err| format!("br show {id} --json: {err}"))?;
        bead_from_json(&value[0]).ok_or_else(|| format!("no bead {id}"))
    }

    fn all_beads(&self) -> Result<Vec<Bead>, String> {
        let out = self.br(&["list", "--all", "--json"])?;
        let value: serde_json::Value =
            serde_json::from_str(&out).map_err(|err| format!("br list --all --json: {err}"))?;
        let issues = value
            .get("issues")
            .unwrap_or(&value)
            .as_array()
            .ok_or("br list --all --json: no issue list")?;
        Ok(issues.iter().filter_map(bead_from_json).collect())
    }

    fn close(&self, id: &str, reason: &str) -> Result<(), String> {
        self.br(&["close", id, "--reason", reason, "--json"])
            .map(drop)
    }

    fn mark_failed(&self, id: &str, comment: &str) -> Result<(), String> {
        self.br(&[
            "update",
            id,
            "--status",
            "open",
            "--add-label",
            "failed",
            "--add-label",
            "needs-owner",
            "--json",
        ])?;
        self.br(&["comments", "add", id, comment, "--json"])
            .map(drop)
    }

    fn flush(&self) -> Result<(), String> {
        self.br(&["sync", "--flush-only"]).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beads_parse_from_br_json() {
        let value = serde_json::json!({
            "id": "br-w07-3", "status": "in_progress", "labels": ["m0", "sub:W07", "accept:DEV-COMMIT"]
        });
        let bead = bead_from_json(&value).unwrap();
        assert_eq!(bead.labels, ["m0", "sub:W07", "accept:DEV-COMMIT"]);
        assert_eq!(bead.status, "in_progress");
        assert_eq!(bead_from_json(&serde_json::json!({"id": "x"})), None);
    }
}
