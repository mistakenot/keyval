//! The O-9 exposure warning: keyval has no authentication (PLAN.md §2.2), so a client or
//! peer address that is not loopback exposes every key to anyone who can reach it
//! (RISK-17). The composition root logs one [`ExposureWarning`] per such address at
//! startup, at [`ExposureWarning::LEVEL`].

use core::fmt;
use std::net::SocketAddr;

use super::Config;
use crate::{LogLevel, NodeId};

/// Which of a node's addresses a warning is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AddressRole {
    /// The client API address.
    Client,
    /// The peer (Raft) address.
    Peer,
}

impl fmt::Display for AddressRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Client => "client",
            Self::Peer => "peer",
        })
    }
}

/// A configured address that is not loopback (O-9).
///
/// Invariant: `address` is not a loopback address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExposureWarning {
    /// The node whose address it is.
    node: NodeId,
    /// Which address.
    role: AddressRole,
    /// The address.
    address: SocketAddr,
}

impl ExposureWarning {
    /// The level to log it at: prominent (O-9).
    pub const LEVEL: LogLevel = LogLevel::Warn;

    /// The node whose address it is.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// Which address.
    #[must_use]
    pub const fn role(&self) -> AddressRole {
        self.role
    }

    /// The address.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }
}

impl fmt::Display for ExposureWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "UNAUTHENTICATED: node {}'s {} address {} is not loopback, and keyval has no \
             authentication: anyone who can reach it can read and write every key (O-9)",
            self.node, self.role, self.address
        )
    }
}

impl Config {
    /// One warning per client or peer address that is not loopback, by node ID then
    /// role. Empty for a loopback-only config such as `config/local.toml` (O-9).
    #[must_use]
    pub fn exposure_warnings(&self) -> Vec<ExposureWarning> {
        self.nodes()
            .flat_map(|node| {
                [
                    (AddressRole::Client, node.client_address()),
                    (AddressRole::Peer, node.peer_address()),
                ]
                .into_iter()
                .filter(|(_, address)| !address.ip().is_loopback())
                .map(|(role, address)| ExposureWarning {
                    node: node.id(),
                    role,
                    address,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{AddressRole, ExposureWarning};
    use crate::{Config, LogLevel, NodeId};

    /// The repository's local cluster config.
    const LOCAL: &str = include_str!("../../../../config/local.toml");

    /// [`LOCAL`] with `from` replaced by `to`, parsed.
    fn local_with(from: &str, to: &str) -> Config {
        assert_eq!(LOCAL.matches(from).count(), 1, "test setup: {from:?} once");
        Config::parse(&LOCAL.replacen(from, to, 1), Path::new("config")).unwrap()
    }

    #[test]
    fn a_loopback_config_produces_no_warning() {
        let config = Config::parse(LOCAL, Path::new("config")).unwrap();
        assert_eq!(
            config.exposure_warnings(),
            [],
            "rule: loopback defaults are quiet (O-9)"
        );
        let ipv6 = local_with("\"127.0.0.1:7003\"", "\"[::1]:7003\"");
        assert_eq!(ipv6.exposure_warnings(), [], "rule: ::1 is loopback too");
    }

    #[test]
    fn a_non_loopback_address_produces_a_prominent_warning() {
        let cases = [
            (
                "\"127.0.0.1:7002\"",
                "\"0.0.0.0:7002\"",
                AddressRole::Client,
                "0.0.0.0:7002",
            ),
            (
                "\"127.0.0.1:8004\"",
                "\"10.0.0.4:8004\"",
                AddressRole::Peer,
                "10.0.0.4:8004",
            ),
            (
                "\"127.0.0.1:7005\"",
                "\"[::]:7005\"",
                AddressRole::Client,
                "[::]:7005",
            ),
        ];
        for (from, to, role, address) in cases {
            let config = local_with(from, to);
            let warnings = config.exposure_warnings();
            assert_eq!(
                warnings.len(),
                1,
                "rule: one warning per non-loopback address (O-9); address={address}"
            );
            let warning: ExposureWarning = warnings[0];
            assert_eq!(
                (warning.role(), warning.address().to_string()),
                (role, address.to_owned())
            );
            assert_eq!(
                ExposureWarning::LEVEL,
                LogLevel::Warn,
                "rule: prominent (O-9)"
            );
            let text = warning.to_string();
            assert!(
                text.contains("UNAUTHENTICATED") && text.contains(address),
                "rule: the warning says the API is unauthenticated and names the address; \
                 text={text}"
            );
            assert!(config.node(warning.node()).is_some());
        }
        let both = local_with(
            "client_address = \"127.0.0.1:7001\"\npeer_address = \"127.0.0.1:8001\"",
            "client_address = \"192.168.1.10:7001\"\npeer_address = \"192.168.1.10:8001\"",
        );
        let summary: Vec<(NodeId, AddressRole)> = both
            .exposure_warnings()
            .iter()
            .map(|warning| (warning.node(), warning.role()))
            .collect();
        assert_eq!(
            summary,
            [
                (NodeId::new(1).unwrap(), AddressRole::Client),
                (NodeId::new(1).unwrap(), AddressRole::Peer)
            ],
            "rule: client and peer addresses are both checked, in order"
        );
    }
}
