//! Targets and per-operation activation (PLAN.md §10.8).
//!
//! A target activates independently per operation: the model server runs every operation
//! (W30 closed); a real target runs a test only once the server wiring for every operation
//! the test exercises has landed — PUT/GET at W18, everything else, and the real cluster as
//! a target, at W32. A test never runs against a target lacking its operations, so a
//! scheduled gate never demands functionality that does not exist yet.

use std::fmt;

use crate::spec::Spec;
use crate::tag::Class;

/// The cluster's voter count: the suite takes this many cluster base URLs.
pub const CLUSTER_SIZE: usize = 5;

/// A kind of target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// The reference model server (§10.14).
    Model,
    /// A real single node.
    SingleNode,
    /// A node of the real five-node cluster.
    ClusterNode,
}

impl Kind {
    /// All kinds.
    pub const ALL: [Self; 3] = [Self::Model, Self::SingleNode, Self::ClusterNode];

    /// Registry spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::SingleNode => "single-node",
            Self::ClusterNode => "cluster",
        }
    }
}

/// Operations a real single node serves: all nine (W18 PUT/GET, W32.1 the rest). Edit this
/// table when the server wiring changes, and only then (§10.8).
const SINGLE_NODE_OPERATIONS: [&str; 9] = [
    "getKey",
    "putKey",
    "deleteKey",
    "compareAndSwap",
    "scanKeys",
    "getRequestStatus",
    "getLiveness",
    "getReadiness",
    "getClusterStatus",
];

/// Operations a real cluster node serves: all nine (W32).
const CLUSTER_OPERATIONS: [&str; 9] = [
    "getKey",
    "putKey",
    "deleteKey",
    "compareAndSwap",
    "scanKeys",
    "getRequestStatus",
    "getLiveness",
    "getReadiness",
    "getClusterStatus",
];

/// Whether a target of `kind` serves `operation`.
#[must_use]
pub fn serves(kind: Kind, operation: &str) -> bool {
    match kind {
        Kind::Model => true,
        Kind::SingleNode => SINGLE_NODE_OPERATIONS.contains(&operation),
        Kind::ClusterNode => CLUSTER_OPERATIONS.contains(&operation),
    }
}

/// The kinds a test of `class` exercising `operations` runs against now.
///
/// Semantic tests
/// run on every kind that serves all their operations; topology tests only on the cluster.
/// A topology test does not run on the cluster until the cluster serves its operations.
#[must_use]
pub fn active_kinds(class: Class, operations: &[&str]) -> Vec<Kind> {
    let candidates: &[Kind] = match class {
        Class::Semantic => &Kind::ALL,
        Class::Topology => &[Kind::ClusterNode],
    };
    candidates
        .iter()
        .copied()
        .filter(|kind| operations.iter().all(|op| serves(*kind, op)))
        .collect()
}

/// A base URL `http://host:port` with no path, parsed once at the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    host: String,
    port: u16,
}

/// Why a base URL or target set was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum TargetError {
    /// Not `http://host:port`.
    BadUrl(String),
    /// The cluster list does not hold exactly [`CLUSTER_SIZE`] URLs.
    ClusterSize(usize),
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadUrl(text) => {
                write!(f, "{text:?} is not a base URL of the form http://host:port")
            }
            Self::ClusterSize(n) => {
                write!(f, "the cluster needs {CLUSTER_SIZE} base URLs, got {n}")
            }
        }
    }
}

impl BaseUrl {
    /// Parses `http://host:port` (no path, query or userinfo).
    ///
    /// # Errors
    /// [`TargetError::BadUrl`] for anything else.
    pub fn parse(text: &str) -> Result<Self, TargetError> {
        let bad = || TargetError::BadUrl(text.to_owned());
        let rest = text.strip_prefix("http://").ok_or_else(bad)?;
        let (host, port) = rest.rsplit_once(':').ok_or_else(bad)?;
        let port: u16 = port.parse().map_err(|_| bad())?;
        let host_ok = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        if !host_ok || port == 0 {
            return Err(bad());
        }
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }

    /// The host name or address.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for BaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "http://{}:{}", self.host, self.port)
    }
}

/// The base URLs a run was given. Absent targets simply do not run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Targets {
    /// The reference model server.
    pub model: Option<BaseUrl>,
    /// A real single node.
    pub single_node: Option<BaseUrl>,
    /// Either empty or exactly [`CLUSTER_SIZE`] nodes.
    cluster: Vec<BaseUrl>,
}

impl Targets {
    /// Builds a target set; `cluster` must be empty or hold exactly five URLs.
    ///
    /// # Errors
    /// [`TargetError::ClusterSize`] otherwise.
    pub fn new(
        model: Option<BaseUrl>,
        single_node: Option<BaseUrl>,
        cluster: Vec<BaseUrl>,
    ) -> Result<Self, TargetError> {
        if !cluster.is_empty() && cluster.len() != CLUSTER_SIZE {
            return Err(TargetError::ClusterSize(cluster.len()));
        }
        Ok(Self {
            model,
            single_node,
            cluster,
        })
    }

    /// Reads `CONFORMANCE_MODEL_URL`, `CONFORMANCE_NODE_URL` and `CONFORMANCE_CLUSTER_URLS`
    /// (five comma-separated URLs) through `get`.
    ///
    /// # Errors
    /// [`TargetError`] for a malformed URL or a cluster list of the wrong size.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, TargetError> {
        let one = |name: &str| get(name).map(|v| BaseUrl::parse(v.trim())).transpose();
        let cluster = get("CONFORMANCE_CLUSTER_URLS")
            .map(|list| {
                list.split(',')
                    .map(|u| BaseUrl::parse(u.trim()))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        Self::new(
            one("CONFORMANCE_MODEL_URL")?,
            one("CONFORMANCE_NODE_URL")?,
            cluster,
        )
    }

    /// The cluster's nodes (empty or five).
    #[must_use]
    pub fn cluster(&self) -> &[BaseUrl] {
        &self.cluster
    }

    /// Every concrete target a test of `class` exercising `operations` runs against now,
    /// in a fixed order: model, single node, cluster nodes 0..5.
    #[must_use]
    pub fn selected(&self, class: Class, operations: &[&str]) -> Vec<(Kind, &BaseUrl)> {
        let mut out = Vec::new();
        for kind in active_kinds(class, operations) {
            match kind {
                Kind::Model => out.extend(self.model.iter().map(|u| (kind, u))),
                Kind::SingleNode => out.extend(self.single_node.iter().map(|u| (kind, u))),
                Kind::ClusterNode => out.extend(self.cluster.iter().map(|u| (kind, u))),
            }
        }
        out
    }
}

/// Names in the activation tables that the spec does not define (a typo would silently
/// deactivate a target forever).
#[must_use]
pub fn unknown_activation_names(spec: &Spec) -> Vec<&'static str> {
    SINGLE_NODE_OPERATIONS
        .iter()
        .chain(CLUSTER_OPERATIONS.iter())
        .copied()
        .filter(|name| spec.operation(name).is_none())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(port: u16) -> BaseUrl {
        BaseUrl::parse(&format!("http://127.0.0.1:{port}")).unwrap()
    }

    #[test]
    fn base_urls_are_parsed_strictly() {
        for bad in [
            "127.0.0.1:80",
            "https://h:1",
            "http://h",
            "http://h:0",
            "http://h:99999",
            "http://:80",
            "http://h:80/x",
            "http://u@h:80",
            "",
        ] {
            assert!(BaseUrl::parse(bad).is_err(), "rule: {bad:?} is refused");
        }
        let ok = BaseUrl::parse("http://localhost:8080").unwrap();
        assert_eq!((ok.host(), ok.port()), ("localhost", 8080));
        assert_eq!(ok.to_string(), "http://localhost:8080");
    }

    #[test]
    fn activation_follows_the_plan_per_operation() {
        assert_eq!(
            active_kinds(Class::Semantic, &["putKey", "scanKeys"]),
            Kind::ALL,
            "rule: at W32 every semantic test runs on model, single node and cluster"
        );
        assert_eq!(
            active_kinds(Class::Semantic, &["putKey", "noSuchOperation"]),
            [Kind::Model],
            "rule: one unserved operation keeps a real target out"
        );
        assert_eq!(
            active_kinds(Class::Topology, &["getClusterStatus"]),
            [Kind::ClusterNode],
            "rule: topology tests run on the cluster only, never the model or a single node"
        );
    }

    #[test]
    fn selection_orders_targets_and_skips_absent_ones() {
        let targets = Targets::new(Some(url(1)), None, Vec::new()).unwrap();
        let picked = targets.selected(Class::Semantic, &["getKey"]);
        assert_eq!(picked.len(), 1, "picked={picked:?}");
        assert_eq!(picked[0].0, Kind::Model);
    }

    #[test]
    fn cluster_has_zero_or_five_nodes() {
        for n in [1, 4, 6] {
            let urls = (1..=n).map(url).collect();
            assert_eq!(
                Targets::new(None, None, urls),
                Err(TargetError::ClusterSize(usize::from(n))),
                "rule: {n} cluster URLs refused"
            );
        }
        assert!(Targets::new(None, None, (1..=5).map(url).collect()).is_ok());
    }

    #[test]
    fn reads_the_environment_shape() {
        let env = |name: &str| match name {
            "CONFORMANCE_MODEL_URL" => Some("http://127.0.0.1:1".to_owned()),
            "CONFORMANCE_CLUSTER_URLS" => {
                Some("http://a:1, http://b:2,http://c:3,http://d:4,http://e:5".to_owned())
            }
            _ => None,
        };
        let targets = Targets::from_lookup(env).unwrap();
        assert!(targets.model.is_some() && targets.single_node.is_none());
        assert_eq!(targets.cluster().len(), 5);
        assert!(Targets::from_lookup(|_| Some("junk".to_owned())).is_err());
    }

    #[test]
    fn activation_tables_name_real_operations() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../api/openapi.yaml");
        let spec = Spec::load(&path).unwrap();
        assert_eq!(unknown_activation_names(&spec), Vec::<&str>::new());
    }
}
