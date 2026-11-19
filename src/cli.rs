//! Command-line surface. Everything here is pure parsing and validation: no
//! filesystem or network access, so it is fully unit-testable.

use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgAction, Args, Parser, Subcommand};

use crate::directory;
use crate::error::{AppError, AppResult};

pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(600);
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Parser)]
#[command(
    name = "portable-opentunnel",
    version,
    about = "Expose one local service through OpenTunnel from a foreground process",
    long_about = "Expose one local service through OpenTunnel.\n\n\
        Works without systemd, root, or an installed OpenTunnel CLI, so it also runs inside Slurm jobs.\n\
        The identity and its random route are saved in the state directory, so the public HTTPS URL \
        stays the same between runs even when the local target changes.",
    after_help = "EXAMPLES:\n  \
        portable-opentunnel 3000 --directory ./my-tunnel\n  \
        portable-opentunnel 127.0.0.1:8080 --directory /shared/private/my-tunnel\n  \
        portable-opentunnel 3000 --ephemeral\n  \
        portable-opentunnel init --directory ./my-tunnel\n  \
        portable-opentunnel url --directory ./my-tunnel\n  \
        portable-opentunnel delete --directory ./my-tunnel\n\n\
        EXIT STATUS:\n  \
        0 success; 1 operational failure; 2 invalid arguments; 3 no initialized identity;\n  \
        130 after SIGINT; 143 after SIGTERM"
)]
pub struct Cli {
    #[command(flatten)]
    pub common: CommonArgs,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Local target: a bare port means 127.0.0.1, or give host:port
    #[arg(value_name = "TARGET")]
    pub target: Option<String>,
}

#[derive(Debug, Args)]
pub struct CommonArgs {
    /// Exact state directory (required for reusable state)
    #[arg(long, short = 'd', global = true, value_name = "DIR")]
    pub directory: Option<PathBuf>,

    /// Use a fresh temporary directory and try to delete the tunnel on exit
    #[arg(long, global = true, conflicts_with = "directory")]
    pub ephemeral: bool,

    /// Extra CA certificate (PEM) to trust for the API and bridge
    #[arg(long, global = true, value_name = "PEM")]
    pub ca_cert: Option<PathBuf>,

    /// Use another API endpoint (for local testing)
    #[arg(long, global = true, hide = true, value_name = "URL")]
    pub api: Option<String>,

    /// Total seconds to wait for provisioning and attachment
    #[arg(long, global = true, value_name = "SECONDS", default_value_t = 600)]
    pub startup_timeout: u64,

    /// Seconds allowed for shutdown, remote deletion, and local cleanup
    #[arg(long, global = true, value_name = "SECONDS", default_value_t = 15)]
    pub shutdown_timeout: u64,

    /// Print more diagnostics (credentials are never printed)
    #[arg(long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create or ensure the saved identity and print its URL without forwarding
    Init,
    /// Print the saved URL, without any network access
    Url,
    /// Delete a saved identity remotely and remove its state
    Delete,
}

/// A fully validated invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Forward(Target),
    Init,
    Url,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub action: Action,
    pub directory: Option<PathBuf>,
    pub ephemeral: bool,
    pub ca_cert: Option<PathBuf>,
    pub api: Option<String>,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub verbose: u8,
}

/// The resolved local target: always `host:port` by the time it reaches the
/// SDK, so a bare port becomes `127.0.0.1:<port>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

impl Target {
    /// The `host:port` string handed to the SDK route. IPv6 hosts are
    /// bracketed, which is what `parse_target` in the SDK accepts.
    pub fn to_route_target(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_route_target())
    }
}

/// Parses `3000`, `127.0.0.1:8080`, `localhost:8080`, `example.test:443`, or
/// `[::1]:8080`.
pub fn parse_target(value: &str) -> AppResult<Target> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::new(2, "the local target is empty"));
    }

    // A bare port, with no host part.
    if !value.contains(':') {
        let port = parse_port(value)?;
        return Ok(Target {
            host: "127.0.0.1".to_owned(),
            port,
        });
    }

    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(|| {
            AppError::new(
                2,
                format!("invalid target '{value}': a bracketed IPv6 address must end with ']'"),
            )
        })?;
        let port = after.strip_prefix(':').ok_or_else(|| {
            AppError::new(2, format!("invalid target '{value}': expected [IPv6]:port"))
        })?;
        if host.is_empty()
            || !host
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() || ch == ':' || ch == '.')
        {
            return Err(AppError::new(
                2,
                format!("invalid target '{value}': '{host}' is not an IPv6 address"),
            ));
        }
        if host.matches(':').count() < 2 && !host.contains('.') {
            return Err(AppError::new(
                2,
                format!("invalid target '{value}': '{host}' is not a full IPv6 address"),
            ));
        }
        (host.to_owned(), port)
    } else {
        let (host, port) = value.rsplit_once(':').ok_or_else(|| {
            AppError::new(
                2,
                format!("invalid target '{value}': use a port or host:port"),
            )
        })?;
        if host.is_empty() {
            return Err(AppError::new(
                2,
                format!("invalid target '{value}': the host is empty"),
            ));
        }
        if host.contains(':') {
            return Err(AppError::new(
                2,
                format!("invalid target '{value}': bracket an IPv6 address as [addr]:port"),
            ));
        }
        if !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        {
            return Err(AppError::new(
                2,
                format!("invalid target '{value}': '{host}' is not a hostname or address"),
            ));
        }
        (host.to_owned(), port)
    };

    let port = parse_port(port)?;
    Ok(Target { host, port })
}

fn parse_port(value: &str) -> AppResult<u16> {
    let port: u16 = value.parse().map_err(|_| {
        AppError::new(
            2,
            format!("invalid port '{value}': expected a number from 1 to 65535"),
        )
    })?;
    if port == 0 {
        return Err(AppError::new(2, "invalid port '0': expected 1 to 65535"));
    }
    Ok(port)
}

impl Cli {
    pub fn resolve(self) -> AppResult<Invocation> {
        let common = self.common;

        let action = match self.command {
            Some(Command::Init) => {
                if common.ephemeral {
                    return Err(AppError::new(2, "init cannot be combined with --ephemeral"));
                }
                if self.target.is_some() {
                    return Err(AppError::new(
                        2,
                        "init does not take a target; it only prepares the identity and route",
                    ));
                }
                Action::Init
            }
            Some(Command::Url) => {
                if common.ephemeral {
                    return Err(AppError::new(2, "url cannot be combined with --ephemeral"));
                }
                if self.target.is_some() {
                    return Err(AppError::new(2, "url does not take a target"));
                }
                Action::Url
            }
            Some(Command::Delete) => {
                if common.ephemeral {
                    return Err(AppError::new(
                        2,
                        "delete cannot be combined with --ephemeral; name the directory with --directory",
                    ));
                }
                if self.target.is_some() {
                    return Err(AppError::new(2, "delete does not take a target"));
                }
                Action::Delete
            }
            None => {
                let target = self.target.ok_or_else(|| {
                    AppError::new(
                        2,
                        "a local target is required: a port such as 3000, or host:port",
                    )
                })?;
                Action::Forward(parse_target(&target)?)
            }
        };

        if !common.ephemeral {
            directory::require_explicit(common.directory.as_deref())?;
        }

        let startup_timeout = Duration::from_secs(common.startup_timeout);
        if startup_timeout.is_zero() {
            return Err(AppError::new(
                2,
                "--startup-timeout must be at least 1 second",
            ));
        }
        let shutdown_timeout = Duration::from_secs(common.shutdown_timeout);
        if shutdown_timeout.is_zero() {
            return Err(AppError::new(
                2,
                "--shutdown-timeout must be at least 1 second",
            ));
        }

        let api = match common.api {
            Some(api) => {
                let url = url::Url::parse(&api)
                    .map_err(|error| AppError::new(2, format!("invalid --api '{api}': {error}")))?;
                if !matches!(url.scheme(), "http" | "https") {
                    return Err(AppError::new(
                        2,
                        format!("invalid --api '{api}': only http and https are supported"),
                    ));
                }
                if url.host().is_none() {
                    return Err(AppError::new(
                        2,
                        format!("invalid --api '{api}': the URL has no host"),
                    ));
                }
                // The SDK joins absolute API paths onto the base, which
                // discards any base path. Accepting one would silently point
                // requests somewhere else, so require a bare origin.
                if !matches!(url.path(), "" | "/")
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err(AppError::new(
                        2,
                        format!(
                            "invalid --api '{api}': use an origin only, such as \
                             http://127.0.0.1:8080, without a path, query, or fragment"
                        ),
                    ));
                }
                Some(api)
            }
            None => None,
        };

        Ok(Invocation {
            action,
            directory: common.directory,
            ephemeral: common.ephemeral,
            ca_cert: common.ca_cert,
            api,
            startup_timeout,
            shutdown_timeout,
            verbose: common.verbose,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Invocation {
        Cli::try_parse_from(args).unwrap().resolve().unwrap()
    }

    #[test]
    fn bare_port_targets_loopback() {
        let invocation = parse(&["portable-opentunnel", "3000", "-d", "./state"]);
        assert_eq!(
            invocation.action,
            Action::Forward(Target {
                host: "127.0.0.1".into(),
                port: 3000
            })
        );
    }

    #[test]
    fn host_and_port_targets_are_kept() {
        for (input, host, port) in [
            ("127.0.0.1:8080", "127.0.0.1", 8080),
            ("localhost:8080", "localhost", 8080),
            ("example.test:443", "example.test", 443),
            ("my_host:51234", "my_host", 51234),
            ("[::1]:8080", "::1", 8080),
            ("[2001:db8::1]:9000", "2001:db8::1", 9000),
        ] {
            let target = parse_target(input).unwrap();
            assert_eq!(target.host, host, "{input}");
            assert_eq!(target.port, port, "{input}");
        }
    }

    #[test]
    fn ipv6_route_targets_are_bracketed() {
        let target = parse_target("[::1]:8080").unwrap();
        assert_eq!(target.to_route_target(), "[::1]:8080");
        let target = parse_target("3000").unwrap();
        assert_eq!(target.to_route_target(), "127.0.0.1:3000");
    }

    #[test]
    fn invalid_targets_are_rejected() {
        for input in [
            "0",
            "65536",
            "abc",
            "-1",
            "",
            ":3000",
            "host:",
            "host:0",
            "host:notaport",
            "[::1]",
            "[::1]8080",
            "::1:8080",
            "host:3000:4000",
            "[zz]:3000",
            "bad host:3000",
        ] {
            assert!(parse_target(input).is_err(), "{input} should be invalid");
        }
    }

    #[test]
    fn ephemeral_uses_no_explicit_directory() {
        let invocation = parse(&["portable-opentunnel", "3000", "--ephemeral"]);
        assert!(invocation.ephemeral);
        assert!(invocation.directory.is_none());
    }

    #[test]
    fn subcommands_reject_incompatible_flags() {
        for args in [
            ["p", "init", "--ephemeral"],
            ["p", "url", "--ephemeral"],
            ["p", "delete", "--ephemeral"],
        ] {
            assert!(
                Cli::try_parse_from(args).unwrap().resolve().is_err(),
                "{args:?}"
            );
        }
        // A subcommand takes no target, so clap rejects it while parsing.
        for args in [
            ["p", "init", "3000"],
            ["p", "url", "3000"],
            ["p", "delete", "3000"],
        ] {
            let error = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{args:?}"
            );
            assert_ne!(error.exit_code(), 0, "{args:?}");
        }
    }

    #[test]
    fn forwarding_requires_a_target() {
        let error = Cli::try_parse_from(["portable-opentunnel"])
            .unwrap()
            .resolve()
            .unwrap_err();
        assert_eq!(error.code, 2);
    }

    #[test]
    fn zero_timeouts_are_rejected() {
        for flag in ["--startup-timeout", "--shutdown-timeout"] {
            let cli = Cli::try_parse_from(["p", "3000", "-d", "./state", flag, "0"]).unwrap();
            let error = cli.resolve().unwrap_err();
            assert_eq!(error.code, 2);
            assert!(error.message.contains(flag), "{error}");
        }
    }

    #[test]
    fn timeouts_are_parsed() {
        let invocation = parse(&[
            "portable-opentunnel",
            "3000",
            "-d",
            "./state",
            "--startup-timeout",
            "30",
            "--shutdown-timeout",
            "5",
        ]);
        assert_eq!(invocation.startup_timeout, Duration::from_secs(30));
        assert_eq!(invocation.shutdown_timeout, Duration::from_secs(5));
    }

    #[test]
    fn api_override_is_validated() {
        for api in ["http://127.0.0.1:1", "http://127.0.0.1:1/"] {
            let invocation = parse(&["p", "3000", "-d", "./state", "--api", api]);
            assert_eq!(invocation.api.as_deref(), Some(api));
        }
        for api in ["ftp://host", "not a url"] {
            let cli = Cli::try_parse_from(["p", "3000", "-d", "./state", "--api", api]).unwrap();
            let error = cli.resolve().unwrap_err();
            assert_eq!(error.code, 2);
            assert!(error.message.contains("invalid --api"), "{error}");
        }
    }

    #[test]
    fn api_override_must_be_an_origin() {
        // The SDK joins absolute paths onto the base, discarding a base path,
        // so a prefix would silently target the wrong endpoint.
        for api in [
            "http://127.0.0.1:8080/prefix",
            "http://127.0.0.1:8080/prefix/",
            "http://127.0.0.1:8080?x=1",
            "http://127.0.0.1:8080#frag",
        ] {
            let cli = Cli::try_parse_from(["p", "3000", "-d", "./state", "--api", api]).unwrap();
            let error = cli.resolve().unwrap_err();
            assert_eq!(error.code, 2, "{api} should be rejected");
            assert!(error.message.contains("origin only"), "{}", error.message);
        }
    }

    #[test]
    fn help_and_version_exit_successfully() {
        for flag in ["--help", "--version"] {
            let error = Cli::try_parse_from(["portable-opentunnel", flag]).unwrap_err();
            assert_eq!(error.exit_code(), 0, "{flag} should exit successfully");
        }
    }
}
