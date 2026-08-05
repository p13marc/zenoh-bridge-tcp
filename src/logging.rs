//! Log sinks and subscriber construction.
//!
//! The bridge writes one event stream to any number of sinks at once. A sink
//! is named by `--log-target`, repeatable, using the same `k=v`-comma grammar
//! as [`crate::spec`]:
//!
//! ```text
//! --log-target stdout | stderr
//! --log-target file=PATH[,rotation=daily|hourly|minutely|never]
//! --log-target journald
//! --log-target syslog[,ident=NAME][,facility=daemon|user|local0..local7]
//! ```
//!
//! `--log-format` shapes only the three [`fmt`](mod@tracing_subscriber::fmt) sinks
//! (stdout/stderr/file). journald and syslog carry the event's fields natively
//! — journald as its own `CLIENT_ID=`/`SERVICE=` fields, which is exactly what
//! capturing our stdout under systemd cannot give you, since that collapses
//! every field into one opaque `MESSAGE=`.
//!
//! Everything composes through a [`Registry`] with a boxed layer per sink;
//! boxing is required because `.json()`/`.compact()` change the layer's type.

use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::str::FromStr;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

/// This crate's own log target prefix.
///
/// It must be spelled out in the default filter, because `EnvFilter` matches
/// targets by plain string prefix rather than by module boundary: a bare
/// `zenoh=warn` directive also matches `zenoh_bridge_tcp::…` and would silence
/// the whole bridge. A longer matching directive wins, so naming ourselves
/// explicitly restores the requested level regardless of directive order.
const SELF_TARGET: &str = "zenoh_bridge_tcp";

/// Dependencies that emit through `tracing` and would otherwise drown the
/// bridge's own output. Zenoh in particular is extremely chatty at `debug`.
///
/// These are a *default*, applied only when `RUST_LOG` is unset — setting
/// `RUST_LOG` hands full control back to the operator.
const NOISY_DEPENDENCIES: &[&str] = &[
    "zenoh",
    "zenoh_ext",
    "zenoh_transport",
    "zenoh_link",
    "zenoh_link_commons",
    "zenoh_runtime",
    "zenoh_util",
    "rustls",
    "tungstenite",
    "tokio_tungstenite",
];

/// How often a `file=` sink rolls over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    /// One file, appended forever. Rotation is someone else's problem
    /// (logrotate, a sidecar); this is the least surprising default.
    #[default]
    Never,
    Daily,
    Hourly,
    Minutely,
}

impl Rotation {
    fn as_str(self) -> &'static str {
        match self {
            Rotation::Never => "never",
            Rotation::Daily => "daily",
            Rotation::Hourly => "hourly",
            Rotation::Minutely => "minutely",
        }
    }
}

/// Syslog facility, restricted to the values that make sense for a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Facility {
    #[default]
    Daemon,
    User,
    Local0,
    Local1,
    Local2,
    Local3,
    Local4,
    Local5,
    Local6,
    Local7,
}

impl Facility {
    fn as_str(self) -> &'static str {
        match self {
            Facility::Daemon => "daemon",
            Facility::User => "user",
            Facility::Local0 => "local0",
            Facility::Local1 => "local1",
            Facility::Local2 => "local2",
            Facility::Local3 => "local3",
            Facility::Local4 => "local4",
            Facility::Local5 => "local5",
            Facility::Local6 => "local6",
            Facility::Local7 => "local7",
        }
    }
}

/// One `--log-target` sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogTarget {
    Stdout,
    Stderr,
    File { path: PathBuf, rotation: Rotation },
    Journald,
    Syslog { ident: String, facility: Facility },
}

/// Default syslog `ident`, matching the binary name systemd would use.
const DEFAULT_SYSLOG_IDENT: &str = "zenoh-bridge-tcp";

impl std::fmt::Display for LogTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogTarget::Stdout => write!(f, "stdout"),
            LogTarget::Stderr => write!(f, "stderr"),
            LogTarget::File { path, rotation } => {
                write!(f, "file={}", path.display())?;
                if *rotation != Rotation::Never {
                    write!(f, ",rotation={}", rotation.as_str())?;
                }
                Ok(())
            }
            LogTarget::Journald => write!(f, "journald"),
            LogTarget::Syslog { ident, facility } => {
                write!(f, "syslog")?;
                if ident != DEFAULT_SYSLOG_IDENT {
                    write!(f, ",ident={ident}")?;
                }
                if *facility != Facility::Daemon {
                    write!(f, ",facility={}", facility.as_str())?;
                }
                Ok(())
            }
        }
    }
}

impl FromStr for LogTarget {
    type Err = anyhow::Error;

    fn from_str(spec: &str) -> Result<Self> {
        let mut parts = spec.split(',');
        let head = parts.next().expect("split yields at least one part");

        // The head is either a bare kind ("stdout") or "file=PATH". Splitting
        // on the first '=' keeps paths containing '=' intact.
        let (kind, head_value) = match head.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (head, None),
        };

        let mut opts: Vec<(&str, &str)> = Vec::new();
        for opt in parts {
            if opt.is_empty() {
                return Err(anyhow::anyhow!(
                    "empty option in --log-target '{spec}' (trailing or doubled comma?)"
                ));
            }
            let (name, value) = opt.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("invalid --log-target option '{opt}': expected 'key=value'")
            })?;
            if value.is_empty() {
                return Err(anyhow::anyhow!(
                    "--log-target option '{name}=' has an empty value"
                ));
            }
            if opts.iter().any(|(n, _)| *n == name) {
                return Err(anyhow::anyhow!("duplicate --log-target option '{name}='"));
            }
            opts.push((name, value));
        }

        // Reject options that belong to a different sink, so a typo like
        // `stdout,rotation=daily` fails loudly instead of being ignored.
        let reject_extra = |opts: &[(&str, &str)], kind: &str| -> Result<()> {
            match opts.first() {
                Some((name, _)) => Err(anyhow::anyhow!(
                    "--log-target {kind} takes no options (got '{name}=')"
                )),
                None => Ok(()),
            }
        };

        match kind {
            "stdout" | "stderr" => {
                if head_value.is_some() {
                    return Err(anyhow::anyhow!("--log-target {kind} takes no value"));
                }
                reject_extra(&opts, kind)?;
                Ok(if kind == "stdout" {
                    LogTarget::Stdout
                } else {
                    LogTarget::Stderr
                })
            }
            "file" => {
                let path = head_value.filter(|v| !v.is_empty()).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--log-target file requires a path: \
                         'file=PATH[,rotation=daily|hourly|minutely|never]'"
                    )
                })?;
                let mut rotation = Rotation::Never;
                for (name, value) in opts {
                    match name {
                        "rotation" => {
                            rotation = match value {
                                "never" => Rotation::Never,
                                "daily" => Rotation::Daily,
                                "hourly" => Rotation::Hourly,
                                "minutely" => Rotation::Minutely,
                                other => {
                                    return Err(anyhow::anyhow!(
                                        "invalid rotation '{other}': expected \
                                         'never', 'daily', 'hourly', or 'minutely'"
                                    ));
                                }
                            };
                        }
                        other => {
                            return Err(anyhow::anyhow!(
                                "unknown --log-target file option '{other}=' (known: rotation=)"
                            ));
                        }
                    }
                }
                Ok(LogTarget::File {
                    path: PathBuf::from(path),
                    rotation,
                })
            }
            "journald" => {
                if head_value.is_some() {
                    return Err(anyhow::anyhow!("--log-target journald takes no value"));
                }
                reject_extra(&opts, "journald")?;
                Ok(LogTarget::Journald)
            }
            "syslog" => {
                if head_value.is_some() {
                    return Err(anyhow::anyhow!(
                        "--log-target syslog takes no value (did you mean ',ident=...'?)"
                    ));
                }
                let mut ident = DEFAULT_SYSLOG_IDENT.to_string();
                let mut facility = Facility::Daemon;
                for (name, value) in opts {
                    match name {
                        "ident" => {
                            if value.contains('\0') {
                                return Err(anyhow::anyhow!(
                                    "syslog ident must not contain a NUL byte"
                                ));
                            }
                            ident = value.to_string();
                        }
                        "facility" => {
                            facility = match value {
                                "daemon" => Facility::Daemon,
                                "user" => Facility::User,
                                "local0" => Facility::Local0,
                                "local1" => Facility::Local1,
                                "local2" => Facility::Local2,
                                "local3" => Facility::Local3,
                                "local4" => Facility::Local4,
                                "local5" => Facility::Local5,
                                "local6" => Facility::Local6,
                                "local7" => Facility::Local7,
                                other => {
                                    return Err(anyhow::anyhow!(
                                        "invalid syslog facility '{other}': expected \
                                         'daemon', 'user', or 'local0'..'local7'"
                                    ));
                                }
                            };
                        }
                        other => {
                            return Err(anyhow::anyhow!(
                                "unknown --log-target syslog option '{other}=' \
                                 (known: ident=, facility=)"
                            ));
                        }
                    }
                }
                Ok(LogTarget::Syslog { ident, facility })
            }
            other => Err(anyhow::anyhow!(
                "unknown --log-target '{other}' (known: stdout, stderr, \
                 file=PATH, journald, syslog)"
            )),
        }
    }
}

/// When to emit ANSI colour escapes.
///
/// `tracing-subscriber` does not tty-detect on its own, so without this a
/// redirected stdout — or a `file=` sink — fills up with escape sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
    /// Colour only when the sink is an interactive terminal.
    #[default]
    Auto,
    Always,
    Never,
}

impl FromStr for ColorChoice {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "auto" => Ok(ColorChoice::Auto),
            "always" => Ok(ColorChoice::Always),
            "never" => Ok(ColorChoice::Never),
            other => Err(anyhow::anyhow!(
                "--log-color must be one of: auto, always, never (got '{other}')"
            )),
        }
    }
}

/// Rendering of the [`fmt`](mod@tracing_subscriber::fmt) sinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Multi-line human-readable rendering, one field per line.
    Pretty,
    /// The default single-line rendering with the span scope inline.
    #[default]
    Full,
    /// Terse single-line rendering.
    Compact,
    /// One JSON object per line, fields and span scope included.
    Json,
}

impl FromStr for LogFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            // `pretty` predates `full` and has always mapped to this
            // formatter; keeping it here avoids reshaping everyone's output.
            "pretty" | "full" => Ok(LogFormat::Full),
            "verbose" => Ok(LogFormat::Pretty),
            "compact" => Ok(LogFormat::Compact),
            "json" => Ok(LogFormat::Json),
            other => Err(anyhow::anyhow!(
                "--log-format must be one of: pretty, full, verbose, compact, json (got '{other}')"
            )),
        }
    }
}

/// Everything [`init`] needs, resolved from the CLI.
#[derive(Debug, Clone)]
pub struct LogOptions {
    pub level: String,
    pub format: LogFormat,
    pub color: ColorChoice,
    pub targets: Vec<LogTarget>,
}

/// Keeps the `tracing-appender` background writer threads alive.
///
/// Dropping this flushes and stops them, so `main` must hold it for the whole
/// process lifetime — an early drop truncates buffered file output.
#[must_use = "dropping the guards stops the background writers and truncates file output"]
pub struct LogGuards(#[allow(dead_code)] Vec<tracing_appender::non_blocking::WorkerGuard>);

/// Build the default filter directive for a level.
///
/// The bridge's own crate gets `level`; noisy dependencies get
/// `min(level, warn)` — quieter than the bridge when we are verbose, but never
/// *louder* than it when we are quiet, so `--log-level error` does not
/// resurrect dependency warnings.
fn default_directives(level: &str) -> String {
    let dep_level = match level {
        "trace" | "debug" | "info" => "warn",
        // warn/error/off: the dependency floor follows the requested level.
        other => other,
    };
    let mut directives = level.to_string();
    if dep_level != level {
        for dep in NOISY_DEPENDENCIES {
            // Infallible: writing into a String.
            let _ = write!(directives, ",{dep}={dep_level}");
        }
        // Undo the prefix collision `zenoh=` causes with our own target.
        let _ = write!(directives, ",{SELF_TARGET}={level}");
    }
    directives
}

/// `RUST_LOG` wins when set and parseable; otherwise the CLI level plus the
/// dependency floor above.
fn build_filter(level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directives(level)))
}

/// Whether a sink should render ANSI, given the choice and whether the sink is
/// a terminal.
fn use_ansi(color: ColorChoice, is_terminal: bool) -> bool {
    match color {
        ColorChoice::Auto => is_terminal,
        ColorChoice::Always => true,
        ColorChoice::Never => false,
    }
}

/// A boxed layer over the shared registry.
type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Build one `fmt` layer over `writer`, in the configured format.
///
/// Each arm produces a differently-typed layer, hence the boxing.
fn fmt_layer<W>(format: LogFormat, ansi: bool, writer: W) -> BoxedLayer
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    let base = tracing_subscriber::fmt::layer()
        .with_ansi(ansi)
        .with_writer(writer);
    match format {
        LogFormat::Pretty => base.pretty().boxed(),
        LogFormat::Full => base.boxed(),
        LogFormat::Compact => base.compact().boxed(),
        // JSON never colours: escape codes inside a JSON string are noise.
        LogFormat::Json => base.with_ansi(false).json().boxed(),
    }
}

/// Install the global subscriber.
///
/// Returns guards that must be held for the process lifetime. Errors rather
/// than panicking, so a bad `file=` path or an unreachable journald socket is
/// reported as a startup failure like any other misconfiguration.
pub fn init(opts: &LogOptions) -> Result<LogGuards> {
    let (subscriber, guards) = build(opts)?;
    subscriber
        .try_init()
        .context("installing the global tracing subscriber")?;
    Ok(guards)
}

/// Assemble the subscriber without installing it.
///
/// Split out from [`init`] so tests can drive the real stack through
/// `tracing::subscriber::with_default` — a global subscriber can only be
/// installed once per process, which would make it untestable otherwise.
fn build(
    opts: &LogOptions,
) -> Result<(
    impl SubscriberInitExt + tracing::Subscriber + Send + Sync,
    LogGuards,
)> {
    let mut guards = Vec::new();
    let mut layers: Vec<BoxedLayer> = Vec::new();

    for target in &opts.targets {
        match target {
            LogTarget::Stdout => {
                let ansi = use_ansi(opts.color, std::io::stdout().is_terminal());
                layers.push(fmt_layer(opts.format, ansi, std::io::stdout));
            }
            LogTarget::Stderr => {
                let ansi = use_ansi(opts.color, std::io::stderr().is_terminal());
                layers.push(fmt_layer(opts.format, ansi, std::io::stderr));
            }
            LogTarget::File { path, rotation } => {
                let (dir, prefix) = split_log_path(path)?;
                // Create the directory eagerly: tracing-appender panics from a
                // background thread otherwise, which is a terrible way to
                // learn about a typo in a path.
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating log directory '{}'", dir.display()))?;
                let appender = match rotation {
                    Rotation::Never => tracing_appender::rolling::never(&dir, &prefix),
                    Rotation::Daily => tracing_appender::rolling::daily(&dir, &prefix),
                    Rotation::Hourly => tracing_appender::rolling::hourly(&dir, &prefix),
                    Rotation::Minutely => tracing_appender::rolling::minutely(&dir, &prefix),
                };
                let (writer, guard) = tracing_appender::non_blocking(appender);
                guards.push(guard);
                // A file is never a terminal, so `always` is the only way to
                // get escapes into it — and then only if explicitly asked.
                let ansi = matches!(opts.color, ColorChoice::Always);
                layers.push(fmt_layer(opts.format, ansi, writer));
            }
            LogTarget::Journald => layers.push(journald_layer()?),
            LogTarget::Syslog { ident, facility } => {
                layers.push(syslog_layer(ident, *facility, opts.format)?)
            }
        }
    }

    // Sinks first, filter second: the boxed layers are typed over `Registry`,
    // so they must be attached to the bare registry. `EnvFilter` as a layer
    // filters globally regardless of its position in the stack.
    let subscriber = Registry::default()
        .with(layers)
        .with(build_filter(&opts.level));

    Ok((subscriber, LogGuards(guards)))
}

/// Split a `file=` path into the directory and file-name halves that
/// `tracing_appender::rolling` wants. A bare file name logs to the cwd.
fn split_log_path(path: &std::path::Path) -> Result<(PathBuf, std::ffi::OsString)> {
    let prefix = path
        .file_name()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--log-target file='{}' has no file name component",
                path.display()
            )
        })?
        .to_os_string();
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok((dir, prefix))
}

#[cfg(target_os = "linux")]
fn journald_layer() -> Result<BoxedLayer> {
    // Fields reach the journal natively: `client_id` becomes `CLIENT_ID=`,
    // queryable with `journalctl CLIENT_ID=<id>`. The crate's default `F_`
    // prefix is dropped so the journal field names match the vocabulary in
    // docs/observability.md verbatim; none of them collide with journald's own
    // well-known fields.
    let layer = tracing_journald::layer()
        .context("connecting to the journald socket (is systemd-journald running?)")?
        .with_field_prefix(None);
    Ok(layer.boxed())
}

#[cfg(not(target_os = "linux"))]
fn journald_layer() -> Result<BoxedLayer> {
    Err(anyhow::anyhow!(
        "--log-target journald is only available on Linux with systemd-journald; \
         use 'file=PATH' or 'stderr' on this platform"
    ))
}

#[cfg(unix)]
fn syslog_layer(ident: &str, facility: Facility, format: LogFormat) -> Result<BoxedLayer> {
    use syslog_tracing::{Facility as SysFacility, Options, Syslog};

    let sys_facility = match facility {
        Facility::Daemon => SysFacility::Daemon,
        Facility::User => SysFacility::User,
        Facility::Local0 => SysFacility::Local0,
        Facility::Local1 => SysFacility::Local1,
        Facility::Local2 => SysFacility::Local2,
        Facility::Local3 => SysFacility::Local3,
        Facility::Local4 => SysFacility::Local4,
        Facility::Local5 => SysFacility::Local5,
        Facility::Local6 => SysFacility::Local6,
        Facility::Local7 => SysFacility::Local7,
    };
    let ident = std::ffi::CString::new(ident)
        .map_err(|_| anyhow::anyhow!("syslog ident must not contain a NUL byte"))?;
    let syslog = Syslog::new(ident, Options::LOG_PID, sys_facility)
        .ok_or_else(|| anyhow::anyhow!("opening the syslog connection failed"))?;

    // syslog(3) prepends its own timestamp and ident, and the message must
    // stay on one line, so drop our timestamp and force a single-line format.
    // Under `json` the whole record is one JSON object, which rsyslog and
    // friends can parse straight out of the message body.
    let ansi = false;
    Ok(match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .with_writer(syslog)
            .json()
            .boxed(),
        _ => tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .with_writer(syslog)
            .without_time()
            .compact()
            .boxed(),
    })
}

#[cfg(not(unix))]
fn syslog_layer(_ident: &str, _facility: Facility, _format: LogFormat) -> Result<BoxedLayer> {
    Err(anyhow::anyhow!(
        "--log-target syslog is only available on Unix; \
         use 'file=PATH' or 'stderr' on this platform"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<LogTarget> {
        s.parse::<LogTarget>()
    }

    #[test]
    fn parses_bare_targets() {
        assert_eq!(parse("stdout").unwrap(), LogTarget::Stdout);
        assert_eq!(parse("stderr").unwrap(), LogTarget::Stderr);
        assert_eq!(parse("journald").unwrap(), LogTarget::Journald);
        assert_eq!(
            parse("syslog").unwrap(),
            LogTarget::Syslog {
                ident: DEFAULT_SYSLOG_IDENT.to_string(),
                facility: Facility::Daemon,
            }
        );
    }

    #[test]
    fn parses_file_target_with_and_without_rotation() {
        assert_eq!(
            parse("file=/var/log/zb.log").unwrap(),
            LogTarget::File {
                path: PathBuf::from("/var/log/zb.log"),
                rotation: Rotation::Never,
            }
        );
        for (spec, want) in [
            ("daily", Rotation::Daily),
            ("hourly", Rotation::Hourly),
            ("minutely", Rotation::Minutely),
            ("never", Rotation::Never),
        ] {
            assert_eq!(
                parse(&format!("file=/tmp/zb.log,rotation={spec}")).unwrap(),
                LogTarget::File {
                    path: PathBuf::from("/tmp/zb.log"),
                    rotation: want,
                }
            );
        }
    }

    #[test]
    fn parses_syslog_options() {
        assert_eq!(
            parse("syslog,ident=bridge,facility=local3").unwrap(),
            LogTarget::Syslog {
                ident: "bridge".to_string(),
                facility: Facility::Local3,
            }
        );
    }

    #[test]
    fn rejects_malformed_targets() {
        // Each error must name the offending piece so the message is
        // actionable without reading the source.
        for (spec, needle) in [
            ("elasticsearch", "unknown --log-target"),
            ("file", "requires a path"),
            ("file=/tmp/a.log,rotation=weekly", "invalid rotation"),
            ("file=/tmp/a.log,rotate=daily", "unknown --log-target file"),
            ("stdout,rotation=daily", "takes no options"),
            ("stdout=/tmp/x", "takes no value"),
            ("journald,ident=x", "takes no options"),
            ("syslog,facility=mail", "invalid syslog facility"),
            ("syslog,color=always", "unknown --log-target syslog option"),
            ("syslog,ident=a,ident=b", "duplicate"),
            ("file=/tmp/a.log,", "empty option"),
            ("file=/tmp/a.log,rotation", "expected 'key=value'"),
            ("file=/tmp/a.log,rotation=", "empty value"),
        ] {
            let err = parse(spec).unwrap_err().to_string().to_ascii_lowercase();
            assert!(
                err.contains(&needle.to_ascii_lowercase()),
                "spec '{spec}': expected an error mentioning '{needle}', got '{err}'"
            );
        }
    }

    #[test]
    fn targets_round_trip_through_display() {
        for spec in [
            "stdout",
            "stderr",
            "journald",
            "syslog",
            "syslog,ident=bridge",
            "syslog,facility=local7",
            "file=/var/log/zb.log",
            "file=/var/log/zb.log,rotation=hourly",
        ] {
            let parsed = parse(spec).unwrap();
            assert_eq!(
                parsed.to_string(),
                spec,
                "'{spec}' did not round-trip through Display"
            );
            assert_eq!(parse(&parsed.to_string()).unwrap(), parsed);
        }
    }

    #[test]
    fn parses_color_choice() {
        assert_eq!("auto".parse::<ColorChoice>().unwrap(), ColorChoice::Auto);
        assert_eq!(
            "always".parse::<ColorChoice>().unwrap(),
            ColorChoice::Always
        );
        assert_eq!("never".parse::<ColorChoice>().unwrap(), ColorChoice::Never);
        assert!(
            "sometimes"
                .parse::<ColorChoice>()
                .unwrap_err()
                .to_string()
                .contains("--log-color")
        );
    }

    #[test]
    fn parses_log_format_including_pretty_alias() {
        // `pretty` is the historical name for the default renderer; keeping it
        // mapped to Full means upgrading does not reshape anyone's output.
        assert_eq!("pretty".parse::<LogFormat>().unwrap(), LogFormat::Full);
        assert_eq!("full".parse::<LogFormat>().unwrap(), LogFormat::Full);
        assert_eq!("verbose".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert_eq!("compact".parse::<LogFormat>().unwrap(), LogFormat::Compact);
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert!(
            "xml"
                .parse::<LogFormat>()
                .unwrap_err()
                .to_string()
                .contains("--log-format")
        );
    }

    #[test]
    fn ansi_follows_the_color_choice() {
        assert!(use_ansi(ColorChoice::Auto, true));
        assert!(!use_ansi(ColorChoice::Auto, false));
        assert!(use_ansi(ColorChoice::Always, false));
        assert!(!use_ansi(ColorChoice::Never, true));
    }

    #[test]
    fn verbose_levels_damp_dependencies_quiet_ones_do_not() {
        // debug: the bridge is verbose, zenoh must not be.
        let d = default_directives("debug");
        assert!(d.starts_with("debug,"));
        assert!(d.contains("zenoh=warn"));
        assert!(d.contains("rustls=warn"));
        // …but our own target must be re-raised, see the regression test below.
        assert!(d.contains("zenoh_bridge_tcp=debug"));

        // error: a dependency floor of `warn` would be *louder* than the
        // bridge, so there is no floor at all.
        assert_eq!(default_directives("error"), "error");
        assert_eq!(default_directives("off"), "off");
        assert_eq!(default_directives("warn"), "warn");
    }

    /// Regression: `EnvFilter` matches targets by plain string *prefix*, not by
    /// module boundary, so a `zenoh=warn` directive also matches
    /// `zenoh_bridge_tcp::…`. Without the explicit self-directive this silenced
    /// the entire bridge and `--log-level info` printed nothing at all.
    #[test]
    fn damping_zenoh_does_not_silence_the_bridge() {
        let out = capture_via_file_sink("info", LogFormat::Full, || {
            tracing::event!(target: "zenoh_bridge_tcp::import", tracing::Level::INFO, "BRIDGE-INFO");
            tracing::event!(target: "zenoh::transport", tracing::Level::INFO, "ZENOH-INFO");
            tracing::event!(target: "zenoh::transport", tracing::Level::WARN, "ZENOH-WARN");
        });
        assert!(out.contains("BRIDGE-INFO"), "bridge was silenced: {out:?}");
        assert!(!out.contains("ZENOH-INFO"), "zenoh was not damped: {out:?}");
        assert!(out.contains("ZENOH-WARN"), "damping went too far: {out:?}");
    }

    #[test]
    fn default_directives_parse_as_a_filter() {
        for level in ["trace", "debug", "info", "warn", "error", "off"] {
            let directives = default_directives(level);
            EnvFilter::try_new(&directives)
                .unwrap_or_else(|e| panic!("'{directives}' is not a valid filter: {e}"));
        }
    }

    /// Drive the real layer stack and return what landed in the `file=` sink.
    ///
    /// Uses `with_default` rather than `init`, so the test does not fight the
    /// other tests over the process-global subscriber.
    fn capture_via_file_sink(opts_level: &str, format: LogFormat, emit: impl FnOnce()) -> String {
        let dir = std::env::temp_dir().join(format!(
            "zbridge-log-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("test.log");
        let opts = LogOptions {
            level: opts_level.to_string(),
            format,
            color: ColorChoice::Never,
            targets: vec![LogTarget::File {
                path: path.clone(),
                rotation: Rotation::Never,
            }],
        };
        let (subscriber, guards) = build(&opts).expect("building the subscriber");
        tracing::subscriber::with_default(subscriber, emit);
        // Dropping the guards flushes and joins the background writer.
        drop(guards);
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        contents
    }

    #[test]
    fn events_reach_the_file_sink_as_json_with_their_fields() {
        let out = capture_via_file_sink("info", LogFormat::Json, || {
            let span = tracing::info_span!("connection", client_id = "abc", service = "svc");
            let _e = span.enter();
            tracing::info!(bytes_up = 12u64, "connection closed");
        });
        let line = out.lines().next().unwrap_or_default().to_string();
        let v: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {e}; line: {line}"));
        assert_eq!(v["fields"]["message"], "connection closed");
        assert_eq!(v["fields"]["bytes_up"], 12);
        // The span's fields ride along, which is what lets call sites stop
        // hand-interpolating client_id into every message.
        assert_eq!(v["span"]["client_id"], "abc");
        assert_eq!(v["span"]["service"], "svc");
    }

    #[test]
    fn the_level_filter_is_applied_to_the_sinks() {
        let out = capture_via_file_sink("warn", LogFormat::Json, || {
            tracing::info!("suppressed");
            tracing::warn!("kept");
        });
        assert!(!out.contains("suppressed"), "got: {out}");
        assert!(out.contains("kept"), "got: {out}");
    }

    #[test]
    fn a_never_color_sink_emits_no_ansi_escapes() {
        let out = capture_via_file_sink("info", LogFormat::Full, || {
            tracing::error!("boom");
        });
        assert!(out.contains("boom"), "got: {out}");
        assert!(!out.contains('\u{1b}'), "ANSI escapes leaked: {out:?}");
    }

    #[test]
    fn splits_log_paths() {
        let (dir, name) = split_log_path(std::path::Path::new("/var/log/zb.log")).unwrap();
        assert_eq!(dir, PathBuf::from("/var/log"));
        assert_eq!(name, "zb.log");

        // A bare file name lands in the working directory.
        let (dir, name) = split_log_path(std::path::Path::new("zb.log")).unwrap();
        assert_eq!(dir, PathBuf::from("."));
        assert_eq!(name, "zb.log");
    }
}
