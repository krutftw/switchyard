//! The process's log subscriber.
//!
//! Three layers on a `tracing_subscriber` registry:
//!
//! 1. a **level filter** that can be replaced at run time (`logging.level`
//!    changes are applied without a restart);
//! 2. a **slot for the telemetry capture layer**, which feeds the
//!    dashboard's Logs page, the event stream and the log files. The
//!    telemetry only exists once the gateway has started, and the gateway
//!    already logs while it starts, so the subscriber is installed first
//!    with the slot empty and the layer is put in afterwards. What the
//!    libraries that *deliver* captured lines say below `warn` is kept out
//!    of the slot at every level (see [`DELIVERY_TARGETS`]);
//! 3. the **stderr writer**: human-readable (coloured when the terminal
//!    supports it) on a terminal, one compact JSON object per line
//!    otherwise.

use std::io::IsTerminal;
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Dispatch, Event, Level, Metadata, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Identity, Layered, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

/// Dependencies that log a lot at `info` and below. They are held at `warn`
/// unless the level is `trace`. (The capture slot holds most of them there
/// at `trace` too: [`DELIVERY_TARGETS`].)
const NOISY_TARGETS: [&str; 8] = [
    "hyper",
    "hyper_util",
    "h2",
    "rustls",
    "tungstenite",
    "tokio_tungstenite",
    "notify",
    "reqwest",
];

/// Dependencies a captured log line passes through on its way out: the
/// HTTP, TLS and WebSocket stack that carries it to a connected dashboard,
/// and the file watcher that sees the log files being written.
///
/// What these say below `warn` never enters the capture slot, whatever the
/// level. At `trace` they describe every frame and every file event, so a
/// captured line that is sent to a dashboard (or written to a log file)
/// would produce new lines, which would be captured and sent in turn: an
/// idle gateway would log about its own log lines without end. On stderr
/// they are still shown, because writing to stderr logs nothing.
const DELIVERY_TARGETS: [&str; 9] = [
    "hyper",
    "hyper_util",
    "h2",
    "rustls",
    "tungstenite",
    "tokio_tungstenite",
    "tokio_util",
    "mio",
    "notify",
];

/// Whether `target` is the crate `name` or a module of it.
fn target_in(target: &str, name: &str) -> bool {
    target
        .strip_prefix(name)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
}

/// Whether the event is chatter (below `warn`) of a delivery dependency.
fn is_delivery_chatter(event: &Event<'_>) -> bool {
    // Events bridged from the `log` crate, which is what the WebSocket
    // library and the file watcher use, carry their real target in a field.
    let bridged = event.normalized_metadata();
    let metadata = bridged.as_ref().unwrap_or_else(|| event.metadata());
    *metadata.level() > Level::WARN
        && DELIVERY_TARGETS
            .iter()
            .any(|name| target_in(metadata.target(), name))
}

/// The capture layer without the chatter of the delivery dependencies.
///
/// A wrapper rather than a per-layer filter: those register themselves with
/// the registry when the subscriber is built, which a layer put into a
/// `reload` slot afterwards cannot do.
struct WithoutDeliveryChatter<L> {
    inner: L,
}

impl<S, L> Layer<S> for WithoutDeliveryChatter<L>
where
    S: Subscriber,
    L: Layer<S>,
{
    fn on_register_dispatch(&self, subscriber: &Dispatch) {
        self.inner.on_register_dispatch(subscriber);
    }

    fn on_layer(&mut self, subscriber: &mut S) {
        self.inner.on_layer(subscriber);
    }

    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        self.inner.register_callsite(metadata)
    }

    fn enabled(&self, metadata: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
        self.inner.enabled(metadata, ctx)
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        self.inner.on_new_span(attrs, id, ctx);
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.inner.max_level_hint()
    }

    fn on_record(&self, span: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        self.inner.on_record(span, values, ctx);
    }

    fn on_follows_from(&self, span: &Id, follows: &Id, ctx: Context<'_, S>) {
        self.inner.on_follows_from(span, follows, ctx);
    }

    fn event_enabled(&self, event: &Event<'_>, ctx: Context<'_, S>) -> bool {
        self.inner.event_enabled(event, ctx)
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Skipped here and not in `event_enabled`: that one decides for the
        // whole subscriber, and stderr still gets these lines.
        if !is_delivery_chatter(event) {
            self.inner.on_event(event, ctx);
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        self.inner.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        self.inner.on_exit(id, ctx);
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        self.inner.on_close(id, ctx);
    }

    fn on_id_change(&self, old: &Id, new: &Id, ctx: Context<'_, S>) {
        self.inner.on_id_change(old, new, ctx);
    }
}

/// The registry with the reloadable level filter on it.
type Filtered = Layered<reload::Layer<EnvFilter, Registry>, Registry>;
/// A layer whose concrete type is decided at run time.
type BoxedLayer<S> = Box<dyn Layer<S> + Send + Sync + 'static>;
/// [`Filtered`] plus the capture slot.
type Captured = Layered<reload::Layer<BoxedLayer<Filtered>, Filtered>, Filtered>;

/// How log lines are written to stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// For people: one line per event, with ANSI colours when `ansi`.
    Human {
        /// Colour the level and dim the timestamp.
        ansi: bool,
    },
    /// For machines: one JSON object per line.
    Json,
}

impl LogFormat {
    /// Human-readable when stderr is a terminal, JSON otherwise.
    pub fn detect() -> Self {
        if std::io::stderr().is_terminal() {
            LogFormat::Human {
                ansi: stderr_supports_ansi(),
            }
        } else {
            LogFormat::Json
        }
    }
}

/// Whether escape sequences written to the (terminal) stderr will be
/// rendered as colours. Honours `NO_COLOR`. On Windows this switches
/// virtual-terminal processing on for the console, and reports false for a
/// console that cannot do it.
fn stderr_supports_ansi() -> bool {
    if anstyle_query::no_color() {
        return false;
    }
    if cfg!(windows) {
        // `enable_ansi_colors` needs both standard handles to be consoles;
        // with stdout redirected, a terminal that announces itself still
        // renders them.
        anstyle_query::windows::enable_ansi_colors() == Some(true)
            || std::env::var_os("WT_SESSION").is_some()
            || anstyle_query::term_supports_ansi_color()
    } else {
        anstyle_query::term_supports_ansi_color()
    }
}

/// The filter to start with, and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterPlan {
    /// `EnvFilter` directives.
    pub directives: String,
    /// The environment decided: `logging.level` changes are not applied.
    pub pinned: bool,
    /// Something to tell the user (an unusable `SWITCHYARD_LOG`).
    pub problem: Option<String>,
}

fn level_name(level: &str) -> Option<&'static str> {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => Some("trace"),
        "debug" => Some("debug"),
        "info" => Some("info"),
        "warn" | "warning" => Some("warn"),
        "error" => Some("error"),
        "off" => Some("off"),
        _ => None,
    }
}

/// Filter directives for a level name. `debug` and `info` hold the noisy
/// dependencies at `warn`; `trace` shows everything; `warn`, `error` and
/// `off` need no exception. An unknown name counts as `info`.
pub fn directives_for(level: &str) -> String {
    let level = level_name(level).unwrap_or("info");
    match level {
        "debug" | "info" => {
            let mut directives = level.to_string();
            for target in NOISY_TARGETS {
                directives.push_str(&format!(",{target}=warn"));
            }
            directives
        }
        other => other.to_string(),
    }
}

/// Decides the initial filter: `SWITCHYARD_LOG` when it is set — a level
/// name, or a full directive string such as
/// `info,switchyard_gateway=debug` — and `logging.level` otherwise.
pub fn plan(config_level: &str, env: Option<&str>) -> FilterPlan {
    let from_config = |problem| FilterPlan {
        directives: directives_for(config_level),
        pinned: false,
        problem,
    };
    let Some(env) = env.map(str::trim).filter(|value| !value.is_empty()) else {
        return from_config(None);
    };
    if level_name(env).is_some() {
        return FilterPlan {
            directives: directives_for(env),
            pinned: true,
            problem: None,
        };
    }
    // A single word that is not a level would be read as "only the target
    // of that name, at trace", which silences everything else: far more
    // likely a misspelt level than what anyone meant.
    if !env.contains(['=', ',']) {
        return from_config(Some(
            "SWITCHYARD_LOG is not a level (trace, debug, info, warn, error, off) or a \
             filter such as \"info,switchyard_gateway=debug\"; using logging.level instead"
                .to_string(),
        ));
    }
    match EnvFilter::try_new(env) {
        Ok(_) => FilterPlan {
            directives: env.to_string(),
            pinned: true,
            problem: None,
        },
        Err(error) => from_config(Some(format!(
            "SWITCHYARD_LOG is neither a level nor a valid filter ({error}); \
             using logging.level instead"
        ))),
    }
}

fn filter_from(directives: &str) -> EnvFilter {
    // Directives built by this module always parse; the fallback keeps the
    // process logging should that ever not hold.
    EnvFilter::try_new(directives).unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Handles on the installed subscriber: change the level, fill the capture
/// slot.
#[derive(Clone)]
pub struct LogControl {
    filter: reload::Handle<EnvFilter, Registry>,
    capture: reload::Handle<BoxedLayer<Filtered>, Filtered>,
    pinned: bool,
}

impl std::fmt::Debug for LogControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogControl")
            .field("pinned", &self.pinned)
            .finish_non_exhaustive()
    }
}

impl LogControl {
    /// Applies a `logging.level` value. Does nothing when `SWITCHYARD_LOG`
    /// decided the filter.
    pub fn set_level(&self, level: &str) {
        if self.pinned {
            return;
        }
        // An error means the subscriber is gone; there is nobody to tell.
        let _ = self.filter.reload(filter_from(&directives_for(level)));
    }

    /// Puts the telemetry's capture layer into its slot, so that log lines
    /// reach the dashboard. Lines below `warn` from the libraries that carry
    /// them there are left out (they would be lines about delivering lines).
    pub fn attach_telemetry(&self, telemetry: &switchyard_telemetry::Telemetry) {
        let layer: BoxedLayer<Filtered> = Box::new(WithoutDeliveryChatter {
            inner: telemetry.log_layer(),
        });
        let _ = self.capture.reload(layer);
    }
}

/// Builds the subscriber without installing it. `writer` receives the
/// formatted lines.
pub fn build<W>(
    format: LogFormat,
    writer: W,
    plan: &FilterPlan,
) -> (impl Subscriber + Send + Sync + 'static, LogControl)
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let (filter, filter_handle) = reload::Layer::new(filter_from(&plan.directives));
    let empty: BoxedLayer<Filtered> = Box::new(Identity::new());
    let (capture, capture_handle) = reload::Layer::new(empty);
    let output: BoxedLayer<Captured> = match format {
        LogFormat::Human { ansi } => tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(ansi)
            .boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .with_writer(writer)
            .boxed(),
    };
    let subscriber = Registry::default().with(filter).with(capture).with(output);
    let control = LogControl {
        filter: filter_handle,
        capture: capture_handle,
        pinned: plan.pinned,
    };
    (subscriber, control)
}

/// Installs the process-wide subscriber, writing to stderr.
///
/// Returns the control handles and, when something is off (an unusable
/// `SWITCHYARD_LOG`, a subscriber that was already installed), a sentence
/// for the user.
pub fn init(config_level: &str, env: Option<&str>) -> (LogControl, Option<String>) {
    let plan = plan(config_level, env);
    let (subscriber, control) = build(LogFormat::detect(), std::io::stderr, &plan);
    let problem = match subscriber.try_init() {
        Ok(()) => plan.problem,
        Err(error) => Some(format!("logging could not be set up: {error}")),
    };
    (control, problem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use switchyard_telemetry::{LogLevel, Telemetry};

    /// Collects what the stderr layer writes.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for Sink {
        type Writer = Sink;

        fn make_writer(&'writer self) -> Sink {
            self.clone()
        }
    }

    fn config_plan(level: &str) -> FilterPlan {
        plan(level, None)
    }

    #[test]
    fn directives_cap_noisy_dependencies() {
        assert_eq!(directives_for("trace"), "trace");
        assert_eq!(directives_for("warn"), "warn");
        assert_eq!(directives_for("ERROR"), "error");
        let info = directives_for("info");
        assert!(info.starts_with("info,"));
        for target in ["hyper", "h2", "rustls", "tungstenite", "notify", "reqwest"] {
            assert!(info.contains(&format!(",{target}=warn")), "{info}");
        }
        assert!(directives_for(" Debug ").starts_with("debug,hyper=warn"));
        // Unknown levels fall back to info.
        assert_eq!(directives_for("loud"), info);
        // Everything this module builds is a valid filter.
        for level in ["trace", "debug", "info", "warn", "error", "off", "?"] {
            assert!(EnvFilter::try_new(directives_for(level)).is_ok(), "{level}");
        }
    }

    #[test]
    fn the_environment_overrides_the_configuration() {
        assert_eq!(
            plan("warn", None),
            FilterPlan {
                directives: "warn".into(),
                pinned: false,
                problem: None
            }
        );
        assert_eq!(plan("warn", Some("  ")).directives, "warn");

        let level = plan("warn", Some("debug"));
        assert!(level.pinned);
        assert!(level.directives.starts_with("debug,hyper=warn"));

        let directives = plan("warn", Some("info,switchyard_gateway=trace"));
        assert!(directives.pinned);
        assert_eq!(directives.directives, "info,switchyard_gateway=trace");

        for unusable in ["switchyard=notalevel", "verbose", "inf0"] {
            let broken = plan("warn", Some(unusable));
            assert!(!broken.pinned, "{unusable}");
            assert_eq!(broken.directives, "warn");
            assert!(broken.problem.unwrap().contains("SWITCHYARD_LOG"));
        }
    }

    #[test]
    fn json_lines_one_object_per_event() {
        let sink = Sink::default();
        let (subscriber, _control) = build(LogFormat::Json, sink.clone(), &config_plan("info"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(port = 8317, "listening");
            tracing::debug!("not shown at info");
            tracing::warn!(provider = "openai", "slow \"upstream\"\nsecond line");
        });
        let text = sink.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["level"], "INFO");
        assert_eq!(first["message"], "listening");
        assert_eq!(first["port"], 8317);
        assert!(first["timestamp"].is_string());
        assert!(first["target"].as_str().unwrap().contains("logging"));
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["level"], "WARN");
        assert_eq!(second["message"], "slow \"upstream\"\nsecond line");
        assert_eq!(second["provider"], "openai");
    }

    #[test]
    fn human_lines_are_plain_without_ansi() {
        let sink = Sink::default();
        let format = LogFormat::Human { ansi: false };
        let (subscriber, _control) = build(format, sink.clone(), &config_plan("info"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(port = 8317, "listening");
        });
        let text = sink.text();
        assert!(text.contains("INFO"), "{text}");
        assert!(text.contains("listening"), "{text}");
        assert!(text.contains("port=8317"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text:?}");

        let coloured = Sink::default();
        let format = LogFormat::Human { ansi: true };
        let (subscriber, _control) = build(format, coloured.clone(), &config_plan("info"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("listening");
        });
        assert!(coloured.text().contains('\u{1b}'));
    }

    #[test]
    fn the_level_can_be_changed_while_running() {
        let sink = Sink::default();
        let (subscriber, control) = build(LogFormat::Json, sink.clone(), &config_plan("info"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("one");
            control.set_level("debug");
            tracing::debug!("two");
            control.set_level("error");
            tracing::warn!("three");
            tracing::error!("four");
            // Not a level: info.
            control.set_level("nonsense");
            tracing::info!("five");
            tracing::debug!("six");
        });
        let text = sink.text();
        for shown in ["two", "four", "five"] {
            assert!(text.contains(&format!("\"message\":\"{shown}\"")), "{text}");
        }
        for hidden in ["one", "three", "six"] {
            assert!(
                !text.contains(&format!("\"message\":\"{hidden}\"")),
                "{text}"
            );
        }
    }

    #[test]
    fn a_pinned_filter_ignores_configuration_changes() {
        let sink = Sink::default();
        let plan = plan("info", Some("warn"));
        let (subscriber, control) = build(LogFormat::Json, sink.clone(), &plan);
        tracing::subscriber::with_default(subscriber, || {
            control.set_level("trace");
            tracing::info!("still hidden");
            tracing::warn!("shown");
        });
        let text = sink.text();
        assert!(!text.contains("still hidden"), "{text}");
        assert!(text.contains("shown"), "{text}");
    }

    #[test]
    fn noisy_dependencies_are_quiet_until_trace() {
        let sink = Sink::default();
        let (subscriber, control) = build(LogFormat::Json, sink.clone(), &config_plan("debug"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "hyper::proto::h1", "chatty");
            tracing::info!(target: "rustls::client", "handshake");
            tracing::warn!(target: "hyper::proto::h1", "important");
            tracing::debug!(target: "switchyard_gateway::generate", "ours");
            control.set_level("trace");
            tracing::trace!(target: "h2::codec", "frame");
        });
        let text = sink.text();
        assert!(!text.contains("chatty"), "{text}");
        assert!(!text.contains("handshake"), "{text}");
        assert!(text.contains("important"), "{text}");
        assert!(text.contains("ours"), "{text}");
        assert!(text.contains("frame"), "{text}");
    }

    #[test]
    fn the_capture_slot_feeds_the_telemetry_once_filled() {
        let sink = Sink::default();
        let telemetry = Telemetry::default();
        let (subscriber, control) = build(LogFormat::Json, sink.clone(), &config_plan("info"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("before the gateway exists");
            control.attach_telemetry(&telemetry);
            tracing::info!(port = 1, "after");
            tracing::debug!("filtered out for both");
        });
        let lines = telemetry.logs().query(10, None, None, None);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].message, "after");
        // stderr got both info lines.
        let text = sink.text();
        assert!(text.contains("before the gateway exists"));
        assert!(text.contains("\"message\":\"after\""));
        assert!(!text.contains("filtered out"));
    }

    fn captured_messages(telemetry: &Telemetry) -> Vec<String> {
        telemetry
            .logs()
            .query(100, None, None, None)
            .into_iter()
            .map(|line| line.message.clone())
            .collect()
    }

    /// At `trace` the WebSocket library logs every frame it sends. Were
    /// those lines captured, sending a captured line to the dashboard would
    /// produce new ones without end.
    #[test]
    fn delivery_chatter_stays_out_of_the_capture_slot() {
        let sink = Sink::default();
        let telemetry = Telemetry::default();
        telemetry.logs().set_min_level(LogLevel::Trace);
        let (subscriber, control) = build(LogFormat::Json, sink.clone(), &config_plan("trace"));
        tracing::subscriber::with_default(subscriber, || {
            control.attach_telemetry(&telemetry);
            for target in DELIVERY_TARGETS {
                tracing::trace!(target: "delivery", name = target, "placeholder");
            }
            tracing::trace!(target: "tungstenite::protocol", "frame written");
            tracing::debug!(target: "tokio_tungstenite", "flushing");
            tracing::info!(target: "h2::codec::framed_write", "send frame");
            tracing::trace!(target: "notify::windows", "file event");
            tracing::trace!(target: "hyper_util::server", "connection");
            // Their warnings and errors are still worth showing.
            tracing::warn!(target: "tungstenite::protocol", "socket warning");
            tracing::error!(target: "rustls::conn", "tls error");
            // Everything else is captured at trace, the HTTP client included.
            tracing::trace!(target: "switchyard_gateway::generate", "our trace");
            tracing::trace!(target: "reqwest::connect", "upstream trace");
            // A crate that merely starts with one of the names is not it.
            tracing::trace!(target: "h2o::server", "another crate");
            tracing::trace!(target: "miow", "yet another");
        });
        let captured = captured_messages(&telemetry);
        for kept in [
            "placeholder",
            "socket warning",
            "tls error",
            "our trace",
            "upstream trace",
            "another crate",
            "yet another",
        ] {
            assert!(captured.iter().any(|m| m == kept), "{kept}: {captured:?}");
        }
        for dropped in [
            "frame written",
            "flushing",
            "send frame",
            "file event",
            "connection",
        ] {
            assert!(!captured.iter().any(|m| m == dropped), "{dropped}");
        }
        // stderr shows all of it: writing there logs nothing.
        let text = sink.text();
        for shown in [
            "frame written",
            "flushing",
            "send frame",
            "file event",
            "connection",
        ] {
            assert!(text.contains(shown), "{shown}: {text}");
        }
    }

    /// The WebSocket library and the file watcher log through the `log`
    /// crate; such events arrive with the target `log` and the real one in
    /// a field.
    #[test]
    fn delivery_chatter_bridged_from_the_log_crate_is_recognised() {
        // Process-wide, like in the binary; an error means it is installed.
        let _ = tracing_log::LogTracer::init();
        let sink = Sink::default();
        let telemetry = Telemetry::default();
        telemetry.logs().set_min_level(LogLevel::Trace);
        let (subscriber, control) = build(LogFormat::Json, sink.clone(), &config_plan("trace"));
        tracing::subscriber::with_default(subscriber, || {
            control.attach_telemetry(&telemetry);
            log::trace!(target: "tokio_tungstenite::compat", "bridged frame");
            log::trace!(target: "notify::windows", "bridged file event");
            log::warn!(target: "tungstenite::protocol", "bridged warning");
            log::trace!(target: "some_other_crate", "bridged elsewhere");
        });
        let captured = captured_messages(&telemetry);
        assert!(
            !captured.iter().any(|m| m == "bridged frame"),
            "{captured:?}"
        );
        assert!(!captured.iter().any(|m| m == "bridged file event"));
        assert!(captured.iter().any(|m| m == "bridged warning"));
        assert!(captured.iter().any(|m| m == "bridged elsewhere"));
        let text = sink.text();
        assert!(text.contains("bridged frame"), "{text}");
        assert!(text.contains("bridged file event"), "{text}");
    }

    /// The level does not matter: a filter from the environment can let the
    /// chatter through at `debug`, and the buffer may record `debug`.
    #[test]
    fn delivery_chatter_is_kept_out_whatever_let_it_through() {
        let sink = Sink::default();
        let telemetry = Telemetry::default();
        telemetry.logs().set_min_level(LogLevel::Debug);
        let plan = plan("debug", Some("info,tungstenite=debug,h2=trace"));
        let (subscriber, control) = build(LogFormat::Json, sink.clone(), &plan);
        tracing::subscriber::with_default(subscriber, || {
            control.attach_telemetry(&telemetry);
            tracing::debug!(target: "tungstenite::handshake", "handshake detail");
            tracing::debug!(target: "h2::proto", "stream detail");
            tracing::info!("ours");
        });
        assert_eq!(captured_messages(&telemetry), ["ours"]);
        assert!(sink.text().contains("handshake detail"));
    }

    #[test]
    fn targets_match_whole_crate_names() {
        assert!(target_in("h2", "h2"));
        assert!(target_in("h2::codec", "h2"));
        assert!(!target_in("h2o", "h2"));
        assert!(!target_in("h", "h2"));
        assert!(!target_in("my::h2", "h2"));
        // Every noisy dependency that carries captured lines is covered.
        for target in ["hyper", "h2", "rustls", "tungstenite", "notify"] {
            assert!(NOISY_TARGETS.contains(&target) && DELIVERY_TARGETS.contains(&target));
        }
    }
}
