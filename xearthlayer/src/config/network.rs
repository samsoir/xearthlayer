//! How hard XEarthLayer pushes the local network (#367).
//!
//! Each limit on network connections is written for capable equipment: up to
//! 1,024 chunk requests in flight on each executor lane, and as many idle
//! connections kept per imagery host. Some home routers and modems drop the
//! whole network when their connection tracking table fills, rather than
//! refusing new connections. `network.connections` lowers every limit by the
//! same fraction, so one setting covers the imagery lanes and the package
//! downloads alike. It is a level, not a number, in keeping with the rule
//! that concurrency is derived rather than tuned.

use std::fmt;
use std::str::FromStr;

/// The values `network.connections` accepts.
pub const NETWORK_CONNECTIONS_VALUES: &[&str] = &["max", "high", "low"];

/// How many of XEarthLayer's usual network connections to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkLevel {
    /// Every limit as designed: up to 1,024 connections per lane, for
    /// prosumer or enterprise network hardware.
    Max,
    /// Two thirds of each limit, for capable home routers. The default.
    #[default]
    High,
    /// One third of each limit.
    Low,
}

impl NetworkLevel {
    /// Every level, fastest first.
    pub const ALL: [NetworkLevel; 3] = [NetworkLevel::Max, NetworkLevel::High, NetworkLevel::Low];

    /// `limit` at this level: all of it, two thirds or one third, rounded up
    /// and never below one, so a limit of one still allows a connection.
    pub fn scale(self, limit: usize) -> usize {
        let thirds = match self {
            NetworkLevel::Max => return limit.max(1),
            NetworkLevel::High => 2,
            NetworkLevel::Low => 1,
        };
        (limit * thirds).div_ceil(3).max(1)
    }

    /// The name `config.ini` uses.
    pub fn name(self) -> &'static str {
        match self {
            NetworkLevel::Max => "max",
            NetworkLevel::High => "high",
            NetworkLevel::Low => "low",
        }
    }
}

impl fmt::Display for NetworkLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for NetworkLevel {
    type Err = String;

    /// Case-insensitive, as every enumerated setting is.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        NetworkLevel::ALL
            .into_iter()
            .find(|level| level.name().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| format!("must be one of: {}", NETWORK_CONNECTIONS_VALUES.join(", ")))
    }
}

impl super::ConfigFile {
    /// Parallel part downloads for package installs: `packages.concurrent_downloads`
    /// at the `network.connections` level. The one place the package manager
    /// and the package commands read it from.
    pub fn package_downloads(&self) -> usize {
        self.network
            .connections
            .scale(self.packages.concurrent_downloads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigFile;

    #[test]
    fn package_downloads_scale_the_configured_value() {
        let mut config = ConfigFile::default();
        config.packages.concurrent_downloads = 5;
        let at = |level| {
            let mut c = config.clone();
            c.network.connections = level;
            c.package_downloads()
        };
        assert_eq!(at(NetworkLevel::Max), 5);
        assert_eq!(at(NetworkLevel::High), 4);
        assert_eq!(at(NetworkLevel::Low), 2);
    }

    #[test]
    fn max_keeps_every_limit() {
        for limit in [1, 5, 10, 1024] {
            assert_eq!(NetworkLevel::Max.scale(limit), limit);
        }
    }

    #[test]
    fn high_is_two_thirds_rounded_up() {
        assert_eq!(NetworkLevel::High.scale(1024), 683);
        assert_eq!(NetworkLevel::High.scale(10), 7);
        assert_eq!(NetworkLevel::High.scale(5), 4);
        assert_eq!(NetworkLevel::High.scale(3), 2);
        assert_eq!(NetworkLevel::High.scale(1), 1);
    }

    #[test]
    fn low_is_one_third_rounded_up() {
        assert_eq!(NetworkLevel::Low.scale(1024), 342);
        assert_eq!(NetworkLevel::Low.scale(10), 4);
        assert_eq!(NetworkLevel::Low.scale(5), 2);
        assert_eq!(NetworkLevel::Low.scale(3), 1);
        assert_eq!(NetworkLevel::Low.scale(1), 1);
    }

    #[test]
    fn no_level_ever_allows_zero_connections() {
        for level in NetworkLevel::ALL {
            assert_eq!(level.scale(0), 1, "{level}");
        }
    }

    #[test]
    fn levels_never_exceed_max_and_are_ordered() {
        for limit in 1..=1024 {
            let (max, high, low) = (
                NetworkLevel::Max.scale(limit),
                NetworkLevel::High.scale(limit),
                NetworkLevel::Low.scale(limit),
            );
            assert!(
                low <= high && high <= max,
                "limit {limit}: {low} {high} {max}"
            );
        }
    }

    #[test]
    fn the_default_is_high() {
        // Max overwhelms many home routers (#353); High is what most users have.
        assert_eq!(NetworkLevel::default(), NetworkLevel::High);
    }

    #[test]
    fn names_parse_back_case_insensitively() {
        for level in NetworkLevel::ALL {
            assert_eq!(level.name().parse::<NetworkLevel>(), Ok(level));
            assert_eq!(
                level.name().to_uppercase().parse::<NetworkLevel>(),
                Ok(level)
            );
        }
        assert_eq!(NETWORK_CONNECTIONS_VALUES, ["max", "high", "low"]);
    }

    #[test]
    fn an_unknown_level_is_refused_with_the_choices() {
        let err = "medium".parse::<NetworkLevel>().unwrap_err();
        assert_eq!(err, "must be one of: max, high, low");
    }
}
