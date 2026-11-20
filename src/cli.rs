//! Command-line surface. Everything here is pure parsing and validation: no
//! filesystem or network access, so it is fully unit-testable.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::output::Log;

const DEFAULT_STARTUP_SECONDS: u64 = 600;
const DEFAULT_SHUTDOWN_SECONDS: u64 = 15;

const BUILD_ID: &str = match option_env!("PORTABLE_OPENTUNNEL_BUILD_ID") {
    Some(id) if !id.is_empty() => id,
    _ => "dev",
};

#[derive(Debug, Parser)]
#[command(
    name = "portable-opentunnel",
    version = BUILD_ID,
    about = "Expose one local service through OpenTunnel from a foreground process",
    long_about = "Expose one local service through OpenTunnel.\n\n\
        Works without systemd, root, or an installed OpenTunnel CLI, so it also runs inside Slurm jobs.\n\
        The identity and its random route are saved in the state directory, so the public HTTPS URL \
        stays the same between runs even when the local target changes.",
    after_help = "EXAMPLES:\n  \
        portable-opentunnel ephemeral 3000\n  \
        portable-opentunnel up 3000 --dir ./my-tunnel\n  \
        portable-opentunnel init --dir ./my-tunnel\n  \
        portable-opentunnel url --dir ./my-tunnel\n  \
        portable-opentunnel delete --dir ./my-tunnel\n\n\
        EXIT STATUS:\n  \
        0 success; 1 operational failure; 2 invalid arguments; 3 no initialized identity;\n  \
        130 after SIGINT; 143 after SIGTERM"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct StateArgs {
    /// Exact state directory
    #[arg(long = "dir", value_name = "DIR")]
    pub directory: PathBuf,
}

#[derive(Debug, Args)]
pub struct NetworkArgs {
    /// Extra CA certificate (PEM) to trust for the API and bridge
    #[arg(long, value_name = "PEM")]
    pub ca_cert: Option<PathBuf>,

    /// API origin of a private OpenTunnel service [default: https://opentunnel.xyz]
    #[arg(long, value_name = "URL", value_parser = parse_api)]
    pub api: Option<String>,

    /// Print more diagnostics (credentials are never printed)
    #[arg(long, overrides_with = "verbose")]
    pub verbose: bool,
}

impl NetworkArgs {
    pub fn log(&self) -> Log {
        Log {
            verbose: self.verbose,
        }
    }
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub network: NetworkArgs,

    /// Total seconds to wait for provisioning and, if forwarding, attachment
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_STARTUP_SECONDS,
        value_parser = clap::value_parser!(u64).range(1..))]
    pub startup_timeout: u64,

    #[command(flatten)]
    pub shutdown: ShutdownArgs,
}

#[derive(Debug, Args)]
pub struct ShutdownArgs {
    /// Seconds allowed for shutdown or deletion
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_SHUTDOWN_SECONDS,
        value_parser = clap::value_parser!(u64).range(1..))]
    pub shutdown_timeout: u64,
}

#[derive(Debug, Args)]
pub struct ForwardArgs {
    /// Local target: a bare port means 127.0.0.1, or give host:port
    #[arg(value_name = "TARGET", value_parser = parse_target)]
    pub target: Target,

    #[command(flatten)]
    pub run: RunArgs,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Forward in the foreground using a saved, reusable URL
    Up {
        #[command(flatten)]
        state: StateArgs,
        #[command(flatten)]
        forward: ForwardArgs,
    },
    /// Forward with a temporary URL and try to delete it on exit
    Ephemeral(ForwardArgs),
    /// Create or ensure the saved identity and print its URL without forwarding
    Init {
        #[command(flatten)]
        state: StateArgs,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Print the saved URL, without any network access
    Url(StateArgs),
    /// Delete a saved identity remotely and remove its state
    Delete {
        #[command(flatten)]
        state: StateArgs,
        #[command(flatten)]
        network: NetworkArgs,
        #[command(flatten)]
        shutdown: ShutdownArgs,
    },
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
pub fn parse_target(value: &str) -> Result<Target, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("the local target is empty".to_owned());
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
            format!("invalid target '{value}': a bracketed IPv6 address must end with ']'")
        })?;
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| format!("invalid target '{value}': expected [IPv6]:port"))?;
        if host.is_empty()
            || !host
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() || ch == ':' || ch == '.')
        {
            return Err(format!(
                "invalid target '{value}': '{host}' is not an IPv6 address"
            ));
        }
        if host.matches(':').count() < 2 && !host.contains('.') {
            return Err(format!(
                "invalid target '{value}': '{host}' is not a full IPv6 address"
            ));
        }
        (host.to_owned(), port)
    } else {
        let (host, port) = value
            .rsplit_once(':')
            .ok_or_else(|| format!("invalid target '{value}': use a port or host:port"))?;
        if host.is_empty() {
            return Err(format!("invalid target '{value}': the host is empty"));
        }
        if host.contains(':') {
            return Err(format!(
                "invalid target '{value}': bracket an IPv6 address as [addr]:port"
            ));
        }
        if !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        {
            return Err(format!(
                "invalid target '{value}': '{host}' is not a hostname or address"
            ));
        }
        (host.to_owned(), port)
    };

    let port = parse_port(port)?;
    Ok(Target { host, port })
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port: u16 = value
        .parse()
        .map_err(|_| format!("invalid port '{value}': expected a number from 1 to 65535"))?;
    if port == 0 {
        return Err("invalid port '0': expected 1 to 65535".to_owned());
    }
    Ok(port)
}

/// Accepts an http(s) origin and returns it normalized (lowercase host, no
/// default port, no trailing slash), so equal origins compare equal.
fn parse_api(api: &str) -> Result<String, String> {
    let url = url::Url::parse(api).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("only http and https are supported".to_owned());
    }
    if url.host().is_none() {
        return Err("the URL has no host".to_owned());
    }
    // The SDK joins absolute API paths onto the base, discarding any prefix.
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err(
            "use an origin only, such as http://127.0.0.1:8080, without a path, query, or fragment"
                .to_owned(),
        );
    }
    Ok(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn parse_ephemeral(args: &[&str]) -> ForwardArgs {
        let Command::Ephemeral(forward) = Cli::try_parse_from(args).unwrap().command else {
            panic!("expected ephemeral");
        };
        forward
    }

    #[test]
    fn bare_port_targets_loopback() {
        let cli = Cli::try_parse_from(["p", "up", "3000", "--dir", "./state"]).unwrap();
        let Command::Up { state, forward } = cli.command else {
            panic!("expected up");
        };
        assert_eq!(state.directory, PathBuf::from("./state"));
        assert_eq!(
            forward.target,
            Target {
                host: "127.0.0.1".into(),
                port: 3000
            }
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
    fn ephemeral_has_no_directory_option() {
        let forward = parse_ephemeral(&["p", "ephemeral", "3000"]);
        assert_eq!(forward.target.port, 3000);
        let error =
            Cli::try_parse_from(["p", "ephemeral", "3000", "--dir", "./state"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn forwarding_requires_a_target() {
        for args in [&["p", "ephemeral"][..], &["p", "up", "--dir", "./state"]] {
            let error = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
            assert!(error.to_string().contains("<TARGET>"), "{error}");
        }
    }

    #[test]
    fn zero_timeouts_are_rejected() {
        for flag in ["--startup-timeout", "--shutdown-timeout"] {
            let error = Cli::try_parse_from(["p", "ephemeral", "3000", flag, "0"]).unwrap_err();
            assert_eq!(error.exit_code(), 2);
            assert!(error.to_string().contains(flag), "{error}");
        }
    }

    #[test]
    fn timeouts_are_parsed() {
        let forward = parse_ephemeral(&[
            "p",
            "ephemeral",
            "3000",
            "--startup-timeout",
            "30",
            "--shutdown-timeout",
            "5",
        ]);
        assert_eq!(forward.run.startup_timeout, 30);
        assert_eq!(forward.run.shutdown.shutdown_timeout, 5);
    }

    #[test]
    fn api_override_is_validated() {
        for (api, origin) in [
            ("http://127.0.0.1:1", "http://127.0.0.1:1"),
            ("http://127.0.0.1:1/", "http://127.0.0.1:1"),
            ("HTTPS://Tunnel.Example:443/", "https://tunnel.example"),
            ("http://[::1]:8080", "http://[::1]:8080"),
        ] {
            let forward = parse_ephemeral(&["p", "ephemeral", "3000", "--api", api]);
            assert_eq!(forward.run.network.api.as_deref(), Some(origin), "{api}");
        }
        for api in ["ftp://host", "not a url"] {
            let error = Cli::try_parse_from(["p", "ephemeral", "3000", "--api", api]).unwrap_err();
            assert_eq!(error.exit_code(), 2);
            assert!(error.to_string().contains("--api"), "{error}");
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
            let error = Cli::try_parse_from(["p", "ephemeral", "3000", "--api", api]).unwrap_err();
            assert_eq!(error.exit_code(), 2, "{api} should be rejected");
            assert!(error.to_string().contains("origin only"), "{error}");
        }
    }

    #[test]
    fn a_repeated_verbose_flag_is_accepted() {
        let cli =
            Cli::try_parse_from(["p", "delete", "--dir", "s", "--verbose", "--verbose"]).unwrap();
        assert!(matches!(cli.command, Command::Delete { network, .. } if network.verbose));
    }

    #[test]
    fn version_reports_the_build_id() {
        let version = Cli::try_parse_from(["p", "--version"]).unwrap_err();
        assert_eq!(
            version.to_string(),
            format!("portable-opentunnel {BUILD_ID}\n")
        );
    }
}
