//! What each client command prints (PLAN.md §8.7, O-4, O-6, C-6; W33.3).
//!
//! The binary only parses arguments; this module runs the commands over a [`Client`] and
//! renders their results twice: as a versioned `--json` document (one schema per command in
//! `schemas/cli/`) and as text for a person. Both say the same thing.
//!
//! - **Exit status.** `0`: the operation reached a terminal success. `1`: it reached a
//!   terminal result that is not a success (`version_mismatch`, `capacity_exceeded`,
//!   `request_id_conflict`, `not_found`, a rejected request, or `history_full` with nothing
//!   submitted). `3`: **unresolved**: the bounds ran out and the write may or may not have
//!   executed. `2` (see [`crate::output`]): the command could not run at all.
//! - **Unresolved is not failed.** A write that ends unresolved names its request ID and the
//!   exact `keyval resume` command; its pending record stays (O-4, RISK-15).
//! - **`resume` never makes an ID.** It replays the persisted record: the same ID, the same
//!   bytes ([`Client::send_write`]).

use std::fmt::Write as _;

use serde_json::{Value as Json, json};
use types::{Decision, Exhausted, Key, RequestId, RequestState, Response, TerminalResult};

use crate::client::http::Transport;
use crate::client::{
    Answer, Client, ClientError, Clock, ClusterReport, Outcome, RandomSource, Rejection, Report,
    StatusReport, Unresolved,
};
use crate::pending::{PendingError, PendingStore, Record};

/// A write's report (`put`, `delete`, `cas`, `resume`).
pub const WRITE_SCHEMA: &str = "keyval.cli.write/v1";
/// `keyval get`'s report.
pub const GET_SCHEMA: &str = "keyval.cli.get/v1";
/// `keyval scan`'s report.
pub const SCAN_SCHEMA: &str = "keyval.cli.scan/v1";
/// `keyval status`'s report.
pub const STATUS_SCHEMA: &str = "keyval.cli.status/v1";
/// `keyval cluster`'s report.
pub const CLUSTER_SCHEMA: &str = "keyval.cli.cluster/v1";
/// `keyval pending`'s report.
pub const PENDING_SCHEMA: &str = "keyval.cli.pending-list/v1";

/// The terminal result was not a success.
pub const EXIT_TERMINAL: u8 = 1;
/// The bounds ran out: the outcome is unknown.
pub const EXIT_UNRESOLVED: u8 = 3;

/// A command's output: the same report as a JSON document and as text, with its exit status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// The `--json` document.
    pub json: Json,
    /// The text for a person.
    pub human: String,
    /// The process exit status.
    pub exit: u8,
}

/// Why a command could not run.
#[derive(Debug)]
pub enum CommandError {
    /// The pending store refused, or holds nothing under the ID.
    Pending(String),
    /// The client could not start.
    Client(ClientError),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending(why) => f.write_str(why),
            Self::Client(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for CommandError {}

impl From<ClientError> for CommandError {
    fn from(err: ClientError) -> Self {
        Self::Client(err)
    }
}

impl From<PendingError> for CommandError {
    fn from(err: PendingError) -> Self {
        Self::Pending(err.to_string())
    }
}

/// The `keyval resume` command line for `id`.
fn resume_hint(id: RequestId) -> String {
    format!("keyval resume {id}")
}

const fn exhausted_name(exhausted: Exhausted) -> &'static str {
    match exhausted {
        Exhausted::Attempts => "attempts",
        Exhausted::Nodes => "nodes",
        Exhausted::Deadline => "deadline",
    }
}

fn rejection_json(rejection: &Rejection) -> Json {
    json!({
        "http_status": rejection.status,
        "code": rejection.code,
        "message": rejection.message,
    })
}

fn unresolved_json(unresolved: &Unresolved) -> Json {
    json!({
        "bound": exhausted_name(unresolved.exhausted),
        "sends": unresolved.sends,
        "nodes_tried": unresolved.nodes_tried.iter().map(|node| node.get()).collect::<Vec<_>>(),
    })
}

/// The name of a terminal write result and whether it is a success.
const fn terminal_name(result: &TerminalResult) -> (&'static str, bool) {
    match result.decision() {
        Decision::Set | Decision::Deleted { .. } => ("applied", true),
        Decision::VersionMismatch { .. } => ("version_mismatch", false),
        Decision::CapacityExceeded { .. } => ("capacity_exceeded", false),
    }
}

/// How a write ended, by name, and whether that is a success.
const fn write_result(answer: &Answer) -> (&'static str, bool) {
    match &answer.response {
        Response::Written { result, .. }
        | Response::Status {
            state: RequestState::Known(result),
            ..
        } => terminal_name(result),
        Response::Conflict { .. } => ("request_id_conflict", false),
        Response::Found { .. }
        | Response::NotFound { .. }
        | Response::Page(_)
        | Response::Status {
            state: RequestState::Unknown,
            ..
        }
        | Response::Refused(_) => ("unexpected", false),
    }
}

fn note_lines(notes: &[String]) -> String {
    notes.iter().fold(String::new(), |mut out, note| {
        let _ = writeln!(out, "note: {note}");
        out
    })
}

/// The report of a write `command` (`put`, `delete`, `cas`, `resume`) under `request_id`.
#[must_use]
pub fn render_write(command: &str, request_id: RequestId, report: &Report) -> Rendered {
    let mut doc = json!({
        "schema": WRITE_SCHEMA,
        "command": command,
        "request_id": request_id.to_string(),
        "result": null,
        "http_status": null,
        "node": null,
        "via_status_lookup": null,
        "server": null,
        "rejection": null,
        "unresolved": null,
        "resume": null,
        "pending_record_left": report.leftover,
        "notes": report.notes,
    });
    let id = request_id;
    let (result, exit, human) = match &report.outcome {
        Outcome::Answered(answer) => {
            doc["http_status"] = json!(answer.http_status);
            doc["node"] = json!(answer.node.get());
            doc["via_status_lookup"] = json!(answer.via_status_lookup);
            doc["server"] = answer.body.clone();
            let (name, ok) = write_result(answer);
            let text = if ok {
                format!(
                    "{name}: request {id} executed (answered by node {})\n",
                    answer.node
                )
            } else {
                format!(
                    "{name}: request {id} reached a terminal result and changed nothing \
                     (answered by node {})\n",
                    answer.node
                )
            };
            (name, if ok { 0 } else { EXIT_TERMINAL }, text)
        }
        Outcome::Rejected(rejection) => {
            doc["rejection"] = rejection_json(rejection);
            (
                "rejected",
                EXIT_TERMINAL,
                format!(
                    "rejected: the server refused request {id} itself ({} {}): {}\n",
                    rejection.status, rejection.code, rejection.message
                ),
            )
        }
        Outcome::Refused { .. } => {
            doc["resume"] = json!(resume_hint(id));
            (
                "refused",
                EXIT_TERMINAL,
                format!(
                    "refused: request {id} was refused with history_full and never submitted; \
                     it did not execute. Its record is kept: {}\n",
                    resume_hint(id)
                ),
            )
        }
        Outcome::Unresolved(unresolved) => {
            doc["unresolved"] = unresolved_json(unresolved);
            doc["resume"] = json!(resume_hint(id));
            (
                "unresolved",
                EXIT_UNRESOLVED,
                format!(
                    "UNRESOLVED: request {id} may or may not have executed ({} sends to {} \
                     nodes; the {} bound ran out). Its outcome is unknown: do not assume it did \
                     not happen. Resume it with the same ID and command: {}\n",
                    unresolved.sends,
                    unresolved.nodes_tried.len(),
                    exhausted_name(unresolved.exhausted),
                    resume_hint(id)
                ),
            )
        }
    };
    doc["result"] = json!(result);
    let mut human = human;
    human.push_str(&note_lines(&report.notes));
    if let Some(left) = &report.leftover {
        let _ = writeln!(
            human,
            "warning: the write is done, but its pending record could not be deleted ({left}); \
             `keyval resume {id}` would replay it harmlessly"
        );
    }
    Rendered {
        json: doc,
        human,
        exit,
    }
}

/// The report of `keyval get`.
#[must_use]
pub fn render_get(key: &str, report: &Report) -> Rendered {
    let mut doc = json!({
        "schema": GET_SCHEMA,
        "key": key,
        "result": null,
        "value": null,
        "version": null,
        "node": null,
        "server": null,
        "rejection": null,
        "unresolved": null,
        "notes": report.notes,
    });
    let (result, exit, mut human) = match &report.outcome {
        Outcome::Answered(answer) => {
            doc["node"] = json!(answer.node.get());
            doc["server"] = answer.body.clone();
            match &answer.response {
                Response::Found { value, version, .. } => {
                    doc["value"] = json!(value.as_str());
                    doc["version"] = json!(version.get());
                    ("found", 0, format!("{}\n", value.as_str()))
                }
                Response::NotFound { .. } => (
                    "not_found",
                    EXIT_TERMINAL,
                    format!("not_found: no value for {key:?}\n"),
                ),
                Response::Written { .. }
                | Response::Conflict { .. }
                | Response::Page(_)
                | Response::Status { .. }
                | Response::Refused(_) => (
                    "unexpected",
                    EXIT_TERMINAL,
                    "unexpected: the answer is not a read's\n".to_owned(),
                ),
            }
        }
        Outcome::Rejected(rejection) => {
            doc["rejection"] = rejection_json(rejection);
            (
                "rejected",
                EXIT_TERMINAL,
                format!(
                    "rejected: {} {}: {}\n",
                    rejection.status, rejection.code, rejection.message
                ),
            )
        }
        Outcome::Refused { .. } => (
            "unexpected",
            EXIT_TERMINAL,
            "unexpected: a read was refused as a write\n".to_owned(),
        ),
        Outcome::Unresolved(unresolved) => {
            doc["unresolved"] = unresolved_json(unresolved);
            (
                "unresolved",
                EXIT_UNRESOLVED,
                format!(
                    "unresolved: no node answered the read within the bounds ({} sends; the {} \
                     bound ran out). A read changes nothing: run it again\n",
                    unresolved.sends,
                    exhausted_name(unresolved.exhausted)
                ),
            )
        }
    };
    doc["result"] = json!(result);
    human.push_str(&note_lines(&report.notes));
    Rendered {
        json: doc,
        human,
        exit,
    }
}

/// The report of one page of `keyval scan`.
#[must_use]
pub fn render_scan(report: &Report) -> Rendered {
    let mut doc = json!({
        "schema": SCAN_SCHEMA,
        "result": null,
        "count": null,
        "has_more": null,
        "next_after": null,
        "node": null,
        "server": null,
        "rejection": null,
        "unresolved": null,
        "notes": report.notes,
    });
    let (result, exit, mut human) = match &report.outcome {
        Outcome::Answered(answer) => {
            doc["node"] = json!(answer.node.get());
            doc["server"] = answer.body.clone();
            if let Response::Page(page) = &answer.response {
                let items = page.items();
                doc["count"] = json!(items.len());
                doc["has_more"] = json!(page.continuation().is_some());
                doc["next_after"] = json!(page.continuation().map(Key::as_str));
                let mut text = String::new();
                for item in items {
                    let _ = writeln!(
                        text,
                        "{}\t{}\t{}",
                        item.key.as_str(),
                        item.version.get(),
                        item.value.as_str()
                    );
                }
                if let Some(next) = page.continuation() {
                    let _ = writeln!(text, "more: pass --after {:?}", next.as_str());
                }
                ("page", 0, text)
            } else {
                (
                    "unexpected",
                    EXIT_TERMINAL,
                    "unexpected: the answer is not a scan page\n".to_owned(),
                )
            }
        }
        Outcome::Rejected(rejection) => {
            doc["rejection"] = rejection_json(rejection);
            (
                "rejected",
                EXIT_TERMINAL,
                format!(
                    "rejected: {} {}: {}\n",
                    rejection.status, rejection.code, rejection.message
                ),
            )
        }
        Outcome::Refused { .. } => (
            "unexpected",
            EXIT_TERMINAL,
            "unexpected: a read was refused as a write\n".to_owned(),
        ),
        Outcome::Unresolved(unresolved) => {
            doc["unresolved"] = unresolved_json(unresolved);
            (
                "unresolved",
                EXIT_UNRESOLVED,
                format!(
                    "unresolved: no node answered the scan within the bounds ({} sends; the {} \
                     bound ran out). A read changes nothing: run it again\n",
                    unresolved.sends,
                    exhausted_name(unresolved.exhausted)
                ),
            )
        }
    };
    doc["result"] = json!(result);
    human.push_str(&note_lines(&report.notes));
    Rendered {
        json: doc,
        human,
        exit,
    }
}

/// The report of `keyval status <request-id>`.
///
/// `unknown` on every node asked is **not** a failure (F-9, P8): the request may be committed
/// and not yet applied there, or still in flight, so it exits `3` and says to resume. A
/// request that is known exits `0` whatever its original result: status reports what
/// happened, it does not judge it.
#[must_use]
pub fn render_status(report: &StatusReport) -> Rendered {
    let id = report.request_id;
    let mut doc = json!({
        "schema": STATUS_SCHEMA,
        "request_id": id.to_string(),
        "state": null,
        "result": null,
        "node": null,
        "server": null,
        "unknown_on": report.unknown_on.iter().map(|node| node.get()).collect::<Vec<_>>(),
        "resume": null,
        "notes": report.notes,
    });
    let (exit, mut human) = if let Some(answer) = &report.known {
        doc["state"] = json!("known");
        doc["node"] = json!(answer.node.get());
        doc["server"] = answer.body.clone();
        let (name, _) = write_result(answer);
        doc["result"] = json!(name);
        (
            0,
            format!(
                "known: request {id} was executed; its original result is {name} (node {})\n",
                answer.node
            ),
        )
    } else {
        doc["state"] = json!("unknown");
        doc["resume"] = json!(resume_hint(id));
        let which = if report.unknown_on.is_empty() {
            "no node could be reached to ask about"
        } else {
            "no node asked has applied"
        };
        (
            EXIT_UNRESOLVED,
            format!(
                "unknown: {which} request {id}. That does not mean it did not happen. If it \
                 is pending here: {}\n",
                resume_hint(id)
            ),
        )
    };
    human.push_str(&note_lines(&report.notes));
    Rendered {
        json: doc,
        human,
        exit,
    }
}

/// The report of `keyval cluster`.
#[must_use]
pub fn render_cluster(report: &ClusterReport) -> Rendered {
    let mut doc = json!({
        "schema": CLUSTER_SCHEMA,
        "result": "unresolved",
        "node": null,
        "server": null,
        "notes": report.notes,
    });
    let (exit, mut human) = match (report.node, &report.body) {
        (Some(node), Some(body)) => {
            doc["result"] = json!("answered");
            doc["node"] = json!(node.get());
            doc["server"] = body.clone();
            (0, format!("node {node} answered:\n{body:#}\n"))
        }
        (None, _) | (_, None) => (
            EXIT_UNRESOLVED,
            "unresolved: no node answered. A read changes nothing: run it again\n".to_owned(),
        ),
    };
    human.push_str(&note_lines(&report.notes));
    Rendered {
        json: doc,
        human,
        exit,
    }
}

/// The report of `keyval pending`: every unresolved write in the store.
#[must_use]
pub fn render_pending(store: &PendingStore, records: &[Record]) -> Rendered {
    let requests: Vec<Json> = records
        .iter()
        .map(|record| {
            json!({
                "request_id": record.request_id.to_string(),
                "method": record.method.as_str(),
                "target": record.target,
                "body_bytes": record.body.len(),
                "resume": resume_hint(record.request_id),
            })
        })
        .collect();
    let doc = json!({
        "schema": PENDING_SCHEMA,
        "store": store.dir().display().to_string(),
        "count": records.len(),
        "requests": requests,
    });
    let mut human = if records.is_empty() {
        "no pending requests\n".to_owned()
    } else {
        format!("{} pending request(s):\n", records.len())
    };
    for record in records {
        let _ = writeln!(
            human,
            "{}  {} {}  ({} body bytes)  resume: {}",
            record.request_id,
            record.method.as_str(),
            record.target,
            record.body.len(),
            resume_hint(record.request_id)
        );
    }
    Rendered {
        json: doc,
        human,
        exit: 0,
    }
}

/// `keyval resume <id>`: sends a pending write again.
///
/// It recovers leftovers of an interrupted persist, loads the record, and sends it with its
/// own ID and exact bytes. It never allocates an ID: if the bounds run out again the request
/// stays pending, unresolved.
///
/// # Errors
///
/// [`CommandError::Pending`] if the store cannot be read or holds no record under `id`;
/// [`CommandError::Client`] if the client cannot start.
pub fn resume<T: Transport, C: Clock, R: RandomSource>(
    client: &mut Client<T, C, R>,
    store: &PendingStore,
    id: RequestId,
) -> Result<Rendered, CommandError> {
    store.recover()?;
    let Some(record) = store.load(id)? else {
        return Err(CommandError::Pending(format!(
            "no pending request {id}: it was never recorded here, or it already reached a \
             terminal result"
        )));
    };
    let report = client.send_write(&record)?;
    Ok(render_write("resume", id, &report))
}

#[cfg(test)]
mod tests;
