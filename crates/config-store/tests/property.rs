//! Property tests of the format-preserving merge, driven by a small
//! deterministic pseudo-random generator: random valid configurations, random
//! starting layouts (with comments sprinkled in) and random sequences of
//! edits.

use serde_json::{Value as Json, json};
use switchyard_config_store::merge::{Strategy, render_update};
use switchyard_core::Config;
use switchyard_core::config::{
    AliasConfig, ClientKey, CredentialConfig, ModelConfig, PayloadRule, PriceConfig,
    ProviderConfig, ProviderKind, RequestLogMode, Strategy as RoutingStrategy, TlsConfig, WireApi,
};
use switchyard_core::protocol::Protocol;
use switchyard_core::reasoning::{Effort, ThinkingSupport};
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

// ---------------------------------------------------------------------------
// Deterministic randomness
// ---------------------------------------------------------------------------

/// xorshift64*: tiny, fast, and the same on every platform.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn unique(&mut self) -> String {
        format!("{:08x}", self.next() as u32)
    }
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// Strings that stress quoting: quotes of both kinds, backslashes, `#`,
/// brackets, non-ASCII, leading/trailing spaces.
const WORDS: [&str; 16] = [
    "alpha",
    "beta-1",
    "two words",
    "it's",
    "say \"hi\"",
    "back\\slash",
    "naïve ☃",
    "#hash",
    "a = b",
    "[brackets]",
    "{braces}",
    "comma, separated",
    "tab\there",
    " padded ",
    "'single'",
    "ünïcödé/ключ",
];

fn word(rng: &mut Rng) -> String {
    rng.pick(&WORDS).to_string()
}

fn secret(rng: &mut Rng) -> String {
    match rng.below(4) {
        0 => format!("env:KEY_{}", rng.unique().to_uppercase()),
        1 => format!("${{KEY_{}}}", rng.unique().to_uppercase()),
        _ => format!("sk-{}{}", rng.unique(), rng.unique()),
    }
}

fn float(rng: &mut Rng) -> f64 {
    *rng.pick(&[
        0.0,
        0.125,
        1.25,
        3.0,
        10.0,
        15.5,
        0.000_001,
        1e21,
        123_456.789,
        0.1,
    ])
}

fn json_value(rng: &mut Rng, depth: usize) -> Json {
    let kinds = if depth >= 3 { 5 } else { 8 };
    match rng.below(kinds) {
        0 => json!(rng.chance(2)),
        1 => json!(rng.below(100_000) as i64 - 50_000),
        2 => json!(float(rng)),
        3 => json!(word(rng)),
        4 => json!(format!("v{}", rng.unique())),
        5 => {
            let n = rng.below(4);
            Json::Array((0..n).map(|_| json_value(rng, depth + 1)).collect())
        }
        6 => {
            // An array of tables, possibly inside an inline table.
            let n = rng.below(3) + 1;
            Json::Array((0..n).map(|i| json!({"id": i, "tag": word(rng)})).collect())
        }
        _ => {
            let n = rng.below(4);
            let mut map = serde_json::Map::new();
            for _ in 0..n {
                map.insert(set_key(rng), json_value(rng, depth + 1));
            }
            Json::Object(map)
        }
    }
}

fn set_key(rng: &mut Rng) -> String {
    rng.pick(&[
        "temperature",
        "reasoning.summary",
        "generationConfig.thinkingConfig.includeThoughts",
        "two words",
        "quo\"te",
        "ключ",
        "metadata.user_id",
        "0",
        "store",
        "top_p",
    ])
    .to_string()
}

fn client_key(rng: &mut Rng) -> ClientKey {
    ClientKey {
        key: if rng.chance(4) {
            format!("env:CLIENT_{}", rng.unique().to_uppercase())
        } else {
            format!("sy-{}", rng.unique())
        },
        name: if rng.chance(2) {
            word(rng)
        } else {
            String::new()
        },
        enabled: !rng.chance(4),
        models: (0..rng.below(3))
            .map(|_| format!("{}-*", rng.unique()))
            .collect(),
        // Zero is refused: no limit is written by leaving the setting out.
        rate_limit_rpm: rng.chance(3).then(|| 1 + rng.below(1000) as u32),
    }
}

fn thinking(rng: &mut Rng) -> ThinkingSupport {
    if rng.chance(2) {
        let n = rng.below(4) + 1;
        ThinkingSupport::levels(&Effort::ALL[..n])
    } else {
        ThinkingSupport {
            min: rng.below(2048) as u32,
            max: 2048 + rng.below(60_000) as u32,
            zero_allowed: rng.chance(2),
            dynamic_allowed: rng.chance(2),
            levels: Vec::new(),
        }
    }
}

fn model(rng: &mut Rng) -> ModelConfig {
    ModelConfig {
        id: format!("model-{}", rng.unique()),
        alias: if rng.chance(2) {
            format!("alias-{}", rng.unique())
        } else {
            String::new()
        },
        display_name: if rng.chance(3) {
            word(rng)
        } else {
            String::new()
        },
        context_window: rng.chance(3).then(|| 1000 + rng.below(1_000_000) as u64),
        max_output_tokens: rng.chance(4).then(|| 100 + rng.below(100_000) as u64),
        thinking: rng.chance(3).then(|| thinking(rng)),
    }
}

fn credential(rng: &mut Rng, kind: ProviderKind) -> CredentialConfig {
    let use_file = kind == ProviderKind::Vertex && rng.chance(2);
    CredentialConfig {
        api_key: if use_file { String::new() } else { secret(rng) },
        label: if rng.chance(2) {
            word(rng)
        } else {
            String::new()
        },
        disabled: rng.chance(5),
        weight: rng.chance(3).then(|| rng.below(10) as u32),
        priority: rng.chance(4).then(|| rng.below(20) as i32 - 10),
        proxy: if rng.chance(5) {
            "socks5://user:pw@127.0.0.1:1080".to_string()
        } else {
            String::new()
        },
        service_account_file: if use_file {
            format!("sa-{}.json", rng.unique())
        } else {
            String::new()
        },
    }
}

/// A header the validation accepts: the name is an HTTP token (which still
/// includes characters a bare TOML key cannot hold), the value any text
/// without control characters.
fn header(rng: &mut Rng) -> (String, String) {
    let name = rng
        .pick(&[
            "X-Title",
            "HTTP-Referer",
            "Authorization",
            "x_api",
            "X.Dotted",
            "X-It's~odd!",
        ])
        .to_string();
    let value = loop {
        let value = word(rng);
        if !value.chars().any(char::is_control) {
            break value;
        }
    };
    (name, value)
}

fn provider(rng: &mut Rng) -> ProviderConfig {
    let kind = *rng.pick(&ProviderKind::ALL);
    let mut p = ProviderConfig::new(format!("p-{}", rng.unique()), kind);
    p.enabled = !rng.chance(5);
    if kind == ProviderKind::OpenaiCompat || rng.chance(4) {
        p.base_url = if kind == ProviderKind::Mock {
            "mock://local".to_string()
        } else {
            format!("https://{}.example.com/v1", rng.unique())
        };
    }
    p.api_keys = (0..rng.below(4)).map(|_| secret(rng)).collect();
    p.credentials = (0..rng.below(3)).map(|_| credential(rng, kind)).collect();
    if rng.chance(3) {
        p.prefix = format!("pre{}", rng.below(100));
    }
    if rng.chance(3) {
        p.priority = rng.below(20) as i32 - 10;
    }
    if rng.chance(6) {
        p.proxy = "direct".to_string();
    }
    for _ in 0..rng.below(3) {
        let (name, value) = header(rng);
        p.headers.insert(name, value);
    }
    p.models = (0..rng.below(4)).map(|_| model(rng)).collect();
    p.exclude = (0..rng.below(3))
        .map(|_| format!("*-{}", rng.unique()))
        .collect();
    p.discover = !rng.chance(4);
    p.wire_api = *rng.pick(&[
        WireApi::Auto,
        WireApi::Auto,
        WireApi::Chat,
        WireApi::Responses,
    ]);
    p.legacy_max_tokens = rng.chance(4).then(|| rng.chance(2));
    p.stream_usage = rng.chance(4).then(|| rng.chance(2));
    if kind == ProviderKind::Vertex {
        p.project = format!("proj-{}", rng.unique());
        p.location = "global".to_string();
    }
    p
}

fn alias(rng: &mut Rng) -> AliasConfig {
    AliasConfig {
        name: format!("alias-{}", rng.unique()),
        targets: (0..rng.below(3) + 1)
            .map(|_| format!("target-{}", rng.unique()))
            .collect(),
        hide_targets: rng.chance(3),
    }
}

fn rule(rng: &mut Rng, filter: bool) -> PayloadRule {
    let mut r = PayloadRule {
        models: (0..rng.below(2) + 1)
            .map(|_| format!("{}*", rng.unique()))
            .collect(),
        protocol: rng.chance(3).then(|| *rng.pick(&Protocol::ALL)),
        provider: if rng.chance(4) {
            format!("p-{}", rng.unique())
        } else {
            String::new()
        },
        ..PayloadRule::default()
    };
    if filter {
        r.remove = (0..rng.below(3) + 1).map(|_| set_key(rng)).collect();
    } else {
        for _ in 0..rng.below(3) + 1 {
            r.set.insert(set_key(rng), json_value(rng, 0));
        }
    }
    r
}

fn price(rng: &mut Rng) -> PriceConfig {
    PriceConfig {
        model: format!("{}*", rng.unique()),
        input: float(rng),
        output: float(rng),
        cache_read: rng.chance(3).then(|| float(rng)),
        cache_write: rng.chance(3).then(|| float(rng)),
    }
}

fn config(rng: &mut Rng) -> Config {
    let mut c = Config::default();
    // Every field keeps its default half of the time, so files with omitted
    // keys and sections are common.
    if rng.chance(2) {
        c.server.host = rng.pick(&["127.0.0.1", "0.0.0.0", "::"]).to_string();
    }
    if rng.chance(2) {
        c.server.port = 1 + rng.below(65_000) as u16;
    }
    if rng.chance(3) {
        c.server.body_limit_mb = 1 + rng.below(512) as u64;
    }
    c.server.cors = !rng.chance(3);
    if rng.chance(3) {
        c.server.data_dir = word(rng);
    }
    if rng.chance(4) {
        c.server.tls = Some(TlsConfig {
            cert: format!("{}.pem", rng.unique()),
            key: word(rng),
        });
    }
    if rng.chance(2) {
        c.admin.secret = secret(rng);
    }
    c.admin.allow_remote = rng.chance(3);
    c.admin.ui = !rng.chance(4);
    c.auth.required = !rng.chance(3);
    c.auth.keys = (0..rng.below(4)).map(|_| client_key(rng)).collect();
    if rng.chance(2) {
        c.routing.strategy = *rng.pick(&[
            RoutingStrategy::RoundRobin,
            RoutingStrategy::FillFirst,
            RoutingStrategy::Weighted,
            RoutingStrategy::LeastLatency,
        ]);
    }
    if rng.chance(3) {
        c.routing.max_attempts = 1 + rng.below(9) as u32;
    }
    if rng.chance(3) {
        c.routing.cooldown.transient_secs = rng.below(600) as u64;
        c.routing.cooldown.enabled = rng.chance(2);
    }
    if rng.chance(3) {
        c.streaming.keepalive_secs = rng.below(120) as u64;
    }
    if rng.chance(4) {
        c.upstream.proxy = "http://proxy.internal:3128".to_string();
    }
    if rng.chance(3) {
        c.logging.level = rng.pick(&["trace", "debug", "warn", "error"]).to_string();
        c.logging.request_log = *rng.pick(&[RequestLogMode::Errors, RequestLogMode::All]);
    }
    if rng.chance(4) {
        c.usage.retention_days = 1 + rng.below(365) as u32;
        c.usage.persist = rng.chance(2);
    }
    c.providers = (0..rng.below(4)).map(|_| provider(rng)).collect();
    c.aliases = (0..rng.below(3)).map(|_| alias(rng)).collect();
    c.payload.default = (0..rng.below(3)).map(|_| rule(rng, false)).collect();
    c.payload.overrides = (0..rng.below(2)).map(|_| rule(rng, false)).collect();
    c.payload.filter = (0..rng.below(2)).map(|_| rule(rng, true)).collect();
    c.pricing = (0..rng.below(3)).map(|_| price(rng)).collect();
    c
}

/// A random valid configuration.
fn valid_config(rng: &mut Rng) -> Config {
    loop {
        let c = config(rng);
        if c.validate().is_empty() {
            return c;
        }
    }
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

/// What an edit did, for the properties that depend on it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// Changed one scalar of a top-level section.
    Scalar,
    /// Only changed the order of elements of one list of tables.
    Reorder,
    /// Only removed elements of a list of tables.
    Removal,
    /// Only added elements to a list of tables.
    Addition,
    Other,
}

fn swap_two<T>(rng: &mut Rng, list: &mut [T]) -> bool {
    if list.len() < 2 {
        return false;
    }
    let a = rng.below(list.len());
    let b = (a + 1 + rng.below(list.len() - 1)) % list.len();
    list.swap(a, b);
    true
}

fn remove_one<T>(rng: &mut Rng, list: &mut Vec<T>) -> bool {
    if list.is_empty() {
        return false;
    }
    list.remove(rng.below(list.len()));
    true
}

fn insert_one<T>(rng: &mut Rng, list: &mut Vec<T>, item: T) {
    let at = rng.below(list.len() + 1);
    list.insert(at, item);
}

/// Applies one random edit. Returns `None` when the chosen edit does not
/// apply to this configuration.
fn mutate(rng: &mut Rng, c: &mut Config) -> Option<Kind> {
    let provider_index = (!c.providers.is_empty()).then(|| rng.below(c.providers.len()));
    Some(match rng.below(30) {
        0 => {
            c.server.port = 1 + rng.below(65_000) as u16;
            Kind::Scalar
        }
        1 => {
            c.server.cors = !c.server.cors;
            Kind::Scalar
        }
        2 => {
            c.server.host = rng
                .pick(&["127.0.0.1", "0.0.0.0", "::", "localhost"])
                .to_string();
            Kind::Scalar
        }
        3 => {
            c.server.tls = match c.server.tls.take() {
                Some(_) if rng.chance(2) => None,
                _ => Some(TlsConfig {
                    cert: format!("{}.pem", rng.unique()),
                    key: format!("{}.key", rng.unique()),
                }),
            };
            Kind::Other
        }
        4 => {
            c.admin.secret = if rng.chance(4) {
                String::new()
            } else {
                secret(rng)
            };
            Kind::Scalar
        }
        5 => {
            c.admin.allow_remote = !c.admin.allow_remote;
            Kind::Scalar
        }
        6 => {
            c.auth.required = !c.auth.required;
            Kind::Scalar
        }
        7 => {
            let key = client_key(rng);
            insert_one(rng, &mut c.auth.keys, key);
            Kind::Addition
        }
        8 => remove_one(rng, &mut c.auth.keys).then_some(Kind::Removal)?,
        9 => swap_two(rng, &mut c.auth.keys).then_some(Kind::Reorder)?,
        10 => {
            let i = (!c.auth.keys.is_empty()).then(|| rng.below(c.auth.keys.len()))?;
            let key = &mut c.auth.keys[i];
            match rng.below(5) {
                0 => key.name = word(rng),
                1 => key.models.push(format!("{}-*", rng.unique())),
                2 => {
                    key.models.pop();
                }
                3 => key.enabled = !key.enabled,
                _ => key.key = format!("sy-{}", rng.unique()),
            }
            Kind::Other
        }
        11 => {
            c.routing.strategy = *rng.pick(&[
                RoutingStrategy::RoundRobin,
                RoutingStrategy::FillFirst,
                RoutingStrategy::Weighted,
                RoutingStrategy::LeastLatency,
            ]);
            Kind::Scalar
        }
        12 => {
            c.routing.cooldown.quota_secs = rng.below(10_000) as u64;
            Kind::Scalar
        }
        13 => {
            match rng.below(4) {
                0 => c.streaming.idle_timeout_secs = rng.below(1000) as u64,
                1 => c.upstream.passthrough_headers = !c.upstream.passthrough_headers,
                2 => c.logging.file = !c.logging.file,
                _ => c.usage.enabled = !c.usage.enabled,
            }
            Kind::Scalar
        }
        14 => {
            let p = provider(rng);
            insert_one(rng, &mut c.providers, p);
            Kind::Addition
        }
        15 => remove_one(rng, &mut c.providers).then_some(Kind::Removal)?,
        16 => swap_two(rng, &mut c.providers).then_some(Kind::Reorder)?,
        17 => {
            let p = &mut c.providers[provider_index?];
            match rng.below(9) {
                0 => p.enabled = !p.enabled,
                1 => {
                    p.prefix = if rng.chance(2) {
                        String::new()
                    } else {
                        format!("pre{}", rng.below(100))
                    }
                }
                2 => p.priority = rng.below(7) as i32 - 3,
                3 => p.discover = !p.discover,
                4 => p.wire_api = *rng.pick(&[WireApi::Auto, WireApi::Chat, WireApi::Responses]),
                5 => p.legacy_max_tokens = rng.pick(&[None, Some(true), Some(false)]).to_owned(),
                6 => p.stream_usage = rng.pick(&[None, Some(true), Some(false)]).to_owned(),
                7 => p.name = format!("renamed-{}", rng.unique()),
                _ => {
                    p.proxy = if p.proxy.is_empty() {
                        "direct".into()
                    } else {
                        String::new()
                    }
                }
            }
            Kind::Other
        }
        18 => {
            let p = &mut c.providers[provider_index?];
            match rng.below(6) {
                0 => p.api_keys.push(secret(rng)),
                1 => {
                    p.api_keys.pop();
                }
                2 => {
                    swap_two(rng, &mut p.api_keys);
                }
                3 => {
                    if let Some(first) = p.api_keys.first_mut() {
                        *first = secret(rng);
                    }
                }
                4 => p.api_keys.insert(0, secret(rng)),
                _ => p.api_keys.clear(),
            }
            Kind::Other
        }
        19 => {
            let p = &mut c.providers[provider_index?];
            match rng.below(3) {
                0 => {
                    let (name, value) = header(rng);
                    p.headers.insert(name, value);
                }
                1 => {
                    if let Some(name) = p.headers.keys().next().cloned() {
                        p.headers.shift_remove(&name);
                    }
                }
                _ => {
                    let (_, replacement) = header(rng);
                    if let Some(value) = p.headers.values_mut().last() {
                        *value = replacement;
                    }
                }
            }
            Kind::Other
        }
        20 => {
            let p = &mut c.providers[provider_index?];
            let m = model(rng);
            insert_one(rng, &mut p.models, m);
            Kind::Addition
        }
        21 => {
            let p = &mut c.providers[provider_index?];
            remove_one(rng, &mut p.models).then_some(Kind::Removal)?
        }
        22 => {
            let p = &mut c.providers[provider_index?];
            swap_two(rng, &mut p.models).then_some(Kind::Reorder)?
        }
        23 => {
            let p = &mut c.providers[provider_index?];
            let i = (!p.models.is_empty()).then(|| rng.below(p.models.len()))?;
            let m = &mut p.models[i];
            match rng.below(5) {
                0 => {
                    m.alias = if rng.chance(2) {
                        String::new()
                    } else {
                        format!("alias-{}", rng.unique())
                    }
                }
                1 => m.display_name = word(rng),
                2 => m.context_window = rng.chance(2).then(|| rng.below(500_000) as u64),
                3 => m.thinking = rng.chance(3).then(|| thinking(rng)),
                _ => m.id = format!("model-{}", rng.unique()),
            }
            Kind::Other
        }
        24 => {
            let p = &mut c.providers[provider_index?];
            match rng.below(4) {
                0 => {
                    let cred = credential(rng, p.kind);
                    insert_one(rng, &mut p.credentials, cred);
                }
                1 => {
                    remove_one(rng, &mut p.credentials);
                }
                2 => {
                    swap_two(rng, &mut p.credentials);
                }
                _ => {
                    if let Some(cred) = p.credentials.last_mut() {
                        cred.label = word(rng);
                        cred.weight = rng.chance(2).then(|| rng.below(9) as u32);
                        cred.disabled = !cred.disabled;
                    }
                }
            }
            Kind::Other
        }
        25 => {
            match rng.below(4) {
                0 => {
                    let a = alias(rng);
                    insert_one(rng, &mut c.aliases, a);
                }
                1 => {
                    remove_one(rng, &mut c.aliases);
                }
                2 => {
                    swap_two(rng, &mut c.aliases);
                }
                _ => {
                    if let Some(a) = c.aliases.first_mut() {
                        a.targets.push(format!("target-{}", rng.unique()));
                        a.hide_targets = !a.hide_targets;
                    }
                }
            }
            Kind::Other
        }
        26 => {
            let filter = rng.chance(3);
            let list = if filter {
                &mut c.payload.filter
            } else if rng.chance(2) {
                &mut c.payload.default
            } else {
                &mut c.payload.overrides
            };
            match rng.below(5) {
                0 => {
                    let r = rule(rng, filter);
                    insert_one(rng, list, r);
                }
                1 => {
                    remove_one(rng, list);
                }
                2 => {
                    swap_two(rng, list);
                }
                3 => {
                    if let Some(r) = list.last_mut() {
                        r.models.push(format!("{}*", rng.unique()));
                        if filter {
                            r.remove.push(set_key(rng));
                        } else {
                            r.set.insert(set_key(rng), json_value(rng, 0));
                        }
                    }
                }
                _ => {
                    if let Some(r) = list.first_mut() {
                        if filter {
                            r.remove.insert(0, set_key(rng));
                        } else if let Some(key) = r.set.keys().next().cloned() {
                            // Change a value in place; nested structures get
                            // merged into whatever is there.
                            r.set.insert(key, json_value(rng, 0));
                        }
                        r.protocol = rng.chance(2).then(|| *rng.pick(&Protocol::ALL));
                    }
                }
            }
            Kind::Other
        }
        27 => {
            match rng.below(4) {
                0 => {
                    let p = price(rng);
                    insert_one(rng, &mut c.pricing, p);
                }
                1 => {
                    remove_one(rng, &mut c.pricing);
                }
                2 => {
                    swap_two(rng, &mut c.pricing);
                }
                _ => {
                    if let Some(p) = c.pricing.first_mut() {
                        p.input = float(rng);
                        p.cache_write = rng.chance(2).then(|| float(rng));
                    }
                }
            }
            Kind::Other
        }
        28 => {
            // Wipe a whole section back to its defaults.
            match rng.below(6) {
                0 => c.routing = Default::default(),
                1 => c.payload = Default::default(),
                2 => c.providers.clear(),
                3 => c.auth = Default::default(),
                4 => c.server = Default::default(),
                _ => c.pricing.clear(),
            }
            Kind::Other
        }
        _ => {
            // Several unrelated edits at once.
            for _ in 0..3 {
                let mut attempt = c.clone();
                if mutate(rng, &mut attempt).is_some() && attempt.validate().is_empty() {
                    *c = attempt;
                }
            }
            Kind::Other
        }
    })
}

/// One random edit that changes the configuration and keeps it valid.
fn edited(rng: &mut Rng, current: &Config) -> (Config, Kind) {
    loop {
        let mut next = current.clone();
        if let Some(kind) = mutate(rng, &mut next)
            && next != *current
            && next.validate().is_empty()
        {
            return (next, kind);
        }
    }
}

// ---------------------------------------------------------------------------
// Starting layouts
// ---------------------------------------------------------------------------

/// Sprinkles comments over a file: whole-line comments and comments at the
/// end of lines. (No generated string contains a line break, so every line
/// boundary is a place where a comment may go.)
fn decorate(rng: &mut Rng, text: &str) -> String {
    let mut out = String::new();
    let mut n = 0;
    for line in text.lines() {
        if rng.chance(6) {
            n += 1;
            if rng.chance(2) {
                out.push('\n');
            }
            out.push_str(&format!("# note {n}\n"));
            if rng.chance(4) {
                out.push('\n');
            }
        }
        out.push_str(line);
        if !line.trim().is_empty() && rng.chance(6) {
            n += 1;
            out.push_str(&format!("   # tail {n}"));
        }
        out.push('\n');
    }
    if rng.chance(3) {
        out.push_str("\n# the end\n");
    }
    out
}

/// Everything spelled out, as a serialiser writes it.
fn pretty(config: &Config) -> String {
    config.to_toml().expect("serialise")
}

/// Only what differs from the defaults, as the merge writes a new file.
fn minimal(config: &Config) -> String {
    render_update("", config).expect("merge into empty").text
}

/// A file in block style (`[section]` / `[[list]]` headers), with or without
/// comments.
fn starting_text(rng: &mut Rng, config: &Config) -> String {
    match rng.below(4) {
        0 => pretty(config),
        1 => minimal(config),
        2 => decorate(rng, &pretty(config)),
        _ => decorate(rng, &minimal(config)),
    }
}

/// Turns some of a table's sub-tables and lists of tables into inline
/// values.
fn inline_children(rng: &mut Rng, table: &mut Table) {
    for (_, item) in table.iter_mut() {
        if !rng.chance(2) {
            continue;
        }
        match std::mem::take(item) {
            Item::Table(child) => {
                *item = Item::Value(TomlValue::InlineTable(child.into_inline_table()));
            }
            Item::ArrayOfTables(children) => {
                *item = Item::Value(TomlValue::Array(children.into_array()));
            }
            other => *item = other,
        }
    }
}

/// Rewrites a file in the other styles TOML allows for the same content:
/// inline tables, inline arrays of tables and dotted keys, at random.
/// Returns `None` when the result does not read back as `config` (the
/// caller then keeps the plain layout).
fn restyle(rng: &mut Rng, text: &str, config: &Config) -> Option<String> {
    let mut doc: DocumentMut = text.parse().ok()?;
    let root = doc.as_table_mut();
    for (_, item) in root.iter_mut() {
        match std::mem::take(item) {
            Item::ArrayOfTables(list) if rng.chance(2) => {
                *item = Item::Value(TomlValue::Array(list.into_array()));
            }
            Item::ArrayOfTables(mut list) => {
                for element in list.iter_mut() {
                    inline_children(rng, element);
                }
                *item = Item::ArrayOfTables(list);
            }
            Item::Table(mut table) => {
                let only_values = table.iter().all(|(_, child)| child.is_value());
                match rng.below(4) {
                    0 => {
                        *item = Item::Value(TomlValue::InlineTable(table.into_inline_table()));
                    }
                    1 if only_values => {
                        // `section.key = value` lines at the top of the file.
                        table.set_dotted(true);
                        *item = Item::Table(table);
                    }
                    _ => {
                        inline_children(rng, &mut table);
                        *item = Item::Table(table);
                    }
                }
            }
            other => *item = other,
        }
    }
    let out = doc.to_string();
    let reparsed: Config = toml::from_str(&out).ok()?;
    (reparsed == *config).then_some(out)
}

/// A file in inline and dotted styles, with or without comments. Falls back
/// to block style for the rare configuration the restyling cannot express.
fn styled_starting_text(rng: &mut Rng, config: &Config) -> String {
    let base = if rng.chance(2) {
        pretty(config)
    } else {
        minimal(config)
    };
    match restyle(rng, &base, config) {
        Some(styled) if rng.chance(2) => decorate(rng, &styled),
        Some(styled) => styled,
        None => base,
    }
}

/// Spreads some inline tables over several lines, as TOML 1.1 allows: one
/// entry per line, with or without a comma after the last one, with comments
/// beside some entries. Nested tables are spread independently.
fn spread_value(rng: &mut Rng, value: &mut TomlValue, depth: usize, notes: &mut usize) {
    match value {
        TomlValue::InlineTable(table) => {
            let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
            for key in &keys {
                if let Some(child) = table.get_mut(key) {
                    spread_value(rng, child, depth + 1, notes);
                }
            }
            if keys.is_empty() || table.is_dotted() || !rng.chance(2) {
                return;
            }
            let indent = "  ".repeat(depth + 1);
            let closing = "  ".repeat(depth);
            let trailing_comma = rng.chance(2);
            // The comment beside an entry is stored in front of the next one.
            let mut beside = String::new();
            for (i, key) in keys.iter().enumerate() {
                if let Some(mut k) = table.key_mut(key) {
                    k.leaf_decor_mut().set_prefix(format!("{beside}\n{indent}"));
                }
                beside = if rng.chance(3) {
                    *notes += 1;
                    format!("  # tail {notes}")
                } else {
                    String::new()
                };
                let last = i + 1 == keys.len();
                if let Some(v) = table.get_mut(key) {
                    v.decor_mut().set_prefix(" ");
                    v.decor_mut().set_suffix(if last && !trailing_comma {
                        format!("{beside}\n{closing}")
                    } else {
                        String::new()
                    });
                }
            }
            table.set_trailing_comma(trailing_comma);
            table.set_trailing(if trailing_comma {
                format!("{beside}\n{closing}")
            } else {
                String::new()
            });
        }
        TomlValue::Array(array) => {
            for element in array.iter_mut() {
                spread_value(rng, element, depth + 1, notes);
            }
        }
        _ => {}
    }
}

fn spread_tables(rng: &mut Rng, table: &mut Table, notes: &mut usize) {
    for (_, item) in table.iter_mut() {
        match item {
            Item::Value(value) => spread_value(rng, value, 0, notes),
            Item::Table(child) => spread_tables(rng, child, notes),
            Item::ArrayOfTables(list) => {
                for child in list.iter_mut() {
                    spread_tables(rng, child, notes);
                }
            }
            Item::None => {}
        }
    }
}

/// A file in inline style whose inline tables span several lines and carry
/// comments.
fn multi_line_starting_text(rng: &mut Rng, config: &Config) -> String {
    let base = styled_starting_text(rng, config);
    let Ok(mut doc) = base.parse::<DocumentMut>() else {
        return base;
    };
    let mut notes = 1000;
    spread_tables(rng, doc.as_table_mut(), &mut notes);
    let out = doc.to_string();
    match toml::from_str::<Config>(&out) {
        Ok(reparsed) if reparsed == *config => out,
        _ => base,
    }
}

fn parse(text: &str) -> Config {
    toml::from_str(text)
        .unwrap_or_else(|e| panic!("output does not parse: {}\n---\n{text}", e.message()))
}

fn non_blank_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.sort_unstable();
    lines
}

/// Lines of `a` that are not in `b` (as multisets).
fn lines_missing<'a>(a: &'a str, b: &str) -> Vec<&'a str> {
    let mut remaining = non_blank_lines(b);
    let mut missing = Vec::new();
    for line in non_blank_lines(a) {
        match remaining.binary_search(&line) {
            Ok(i) => {
                remaining.remove(i);
            }
            Err(_) => missing.push(line),
        }
    }
    missing
}

fn comment_count(text: &str) -> usize {
    text.matches("# note ").count() + text.matches("# tail ").count()
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

#[test]
fn generated_configs_are_valid_and_varied() {
    let mut rng = Rng::new(7);
    let mut with_providers = 0;
    for _ in 0..200 {
        let c = valid_config(&mut rng);
        assert!(c.validate().is_empty());
        with_providers += usize::from(!c.providers.is_empty());
        // The generator's output survives a plain serialisation round trip,
        // otherwise the properties below would test the generator.
        assert_eq!(parse(&c.to_toml().unwrap()), c);
    }
    assert!(with_providers > 100);

    // The generators themselves only produce what the validation accepts —
    // header names and values, rate limits, thinking ranges, alias names
    // and targets, prices — so `valid_config` throws nothing away and the
    // properties cover every generated shape.
    let mut rng = Rng::new(11);
    for case in 0..500 {
        let c = config(&mut rng);
        let issues: Vec<String> = c.validate().iter().map(ToString::to_string).collect();
        assert!(issues.is_empty(), "case {case}: {issues:?}");
    }
}

/// Runs `cases` random edits, in sequences of one to five per starting file,
/// checking after each that the rewritten text was merged (not rewritten from
/// scratch), parses to exactly the intended configuration, invents no
/// comment, and is stable. Returns how many starting files used inline or
/// dotted style.
fn round_trip(seed: u64, cases: usize, layout: fn(&mut Rng, &Config) -> String) -> usize {
    let mut rng = Rng::new(seed);
    let mut done = 0;
    let mut styled = 0;
    while done < cases {
        let mut current = valid_config(&mut rng);
        let mut text = layout(&mut rng, &current);
        styled += usize::from(
            text.contains("= {") || text.contains("= [{") || text.starts_with("server."),
        );
        assert_eq!(parse(&text), current, "starting text is wrong:\n{text}");

        // A no-op update leaves the file byte-identical.
        let same = render_update(&text, &current).unwrap();
        assert_eq!(same.strategy, Strategy::Unchanged);
        assert_eq!(same.text, text);

        for _ in 0..rng.below(5) + 1 {
            let (next, kind) = edited(&mut rng, &current);
            let rendered = render_update(&text, &next)
                .unwrap_or_else(|e| panic!("case {done} ({kind:?}): {e}\n---\n{text}"));
            assert_eq!(
                rendered.strategy,
                Strategy::Merged,
                "case {done} ({kind:?}) was not merged\n--- before\n{text}\n--- after\n{}",
                rendered.text
            );
            assert_eq!(
                parse(&rendered.text),
                next,
                "case {done} ({kind:?})\n--- before\n{text}\n--- after\n{}",
                rendered.text
            );
            // No comment is ever duplicated or invented.
            assert!(
                comment_count(&rendered.text) <= comment_count(&text),
                "case {done} ({kind:?})\n--- before\n{text}\n--- after\n{}",
                rendered.text
            );
            // And the result is stable: a no-op on it changes nothing.
            let again = render_update(&rendered.text, &next).unwrap();
            assert_eq!(again.strategy, Strategy::Unchanged);
            assert_eq!(again.text, rendered.text);

            text = rendered.text;
            current = next;
            done += 1;
        }
    }
    styled
}

// The 2,000 random cases are split over two tests by starting layout, which
// also lets them run in parallel.

#[test]
fn random_edit_sequences_round_trip_in_block_layouts() {
    round_trip(0x5EED_CAFE, 1000, starting_text);
}

#[test]
fn random_edit_sequences_round_trip_in_inline_and_dotted_layouts() {
    let styled = round_trip(0x0D07_7ED5, 1000, styled_starting_text);
    // The restyling really produced inline tables, inline lists and dotted
    // keys.
    assert!(styled > 150, "{styled}");
}

/// Inline tables written over several lines (TOML 1.1), with comments beside
/// their entries: the layouts the first two tests do not produce.
#[test]
fn random_edit_sequences_round_trip_in_multi_line_inline_layouts() {
    // The layout really produces what it is meant to.
    let mut rng = Rng::new(0x51AB);
    let mut spread = 0;
    let mut commented = 0;
    for _ in 0..200 {
        let config = valid_config(&mut rng);
        let text = multi_line_starting_text(&mut rng, &config);
        assert_eq!(parse(&text), config, "{text}");
        spread += usize::from(text.contains("{\n"));
        commented += usize::from(text.contains("# tail 10"));
    }
    assert!(spread > 60 && commented > 30, "{spread} {commented}");

    round_trip(0x3A11_7AB1, 800, multi_line_starting_text);
}

#[test]
fn edits_touch_only_what_they_change() {
    let mut rng = Rng::new(0xD1FF);
    let mut seen = [0usize; 4];
    let mut cases = 0;
    while cases < 1200 {
        let mut current = valid_config(&mut rng);
        let mut text = starting_text(&mut rng, &current);
        for _ in 0..3 {
            let (next, kind) = edited(&mut rng, &current);
            let rendered = render_update(&text, &next).unwrap();
            assert_eq!(rendered.strategy, Strategy::Merged);
            let out = rendered.text;
            let removed = lines_missing(&text, &out);
            let added = lines_missing(&out, &text);
            let context = || {
                format!(
                    "{kind:?}\n--- before\n{text}\n--- after\n{out}\n--- removed {removed:?}\n--- added {added:?}"
                )
            };
            match kind {
                Kind::Scalar => {
                    // One line changes, or one key (plus, at most, the
                    // headers of a section and its parent) appears.
                    assert!(removed.len() <= 1, "{}", context());
                    assert!(added.len() <= 3, "{}", context());
                    assert_eq!(comment_count(&out), comment_count(&text), "{}", context());
                    seen[0] += 1;
                }
                Kind::Reorder => {
                    // Blocks move; no line is rewritten, lost or invented.
                    assert!(removed.is_empty() && added.is_empty(), "{}", context());
                    assert_eq!(comment_count(&out), comment_count(&text), "{}", context());
                    seen[1] += 1;
                }
                Kind::Removal => {
                    assert!(added.is_empty(), "{}", context());
                    seen[2] += 1;
                }
                Kind::Addition => {
                    assert!(removed.is_empty(), "{}", context());
                    assert_eq!(comment_count(&out), comment_count(&text), "{}", context());
                    seen[3] += 1;
                }
                Kind::Other => {}
            }
            drop(removed);
            drop(added);
            text = out;
            current = next;
            cases += 1;
        }
    }
    // Every property was actually exercised.
    assert!(seen.iter().all(|&n| n > 30), "{seen:?}");
}

// ---------------------------------------------------------------------------
// Banners
// ---------------------------------------------------------------------------

/// A header of a banner-decorated file: the dotted path between its brackets
/// and whether it opens a list element (`[[…]]`).
struct Header {
    path: String,
    element: bool,
}

/// Puts a comment paragraph that stands apart — `# banner N`, with a blank
/// line on both sides — above every header of a block-style file. Returns the
/// text and the headers, banner `N` being the one above header `N`.
fn with_banners(text: &str) -> (String, Vec<Header>) {
    let mut out = String::new();
    let mut headers = Vec::new();
    for line in text.lines() {
        if line.starts_with('[') {
            let element = line.starts_with("[[");
            let path = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(&format!("# banner {}\n\n", headers.len()));
            headers.push(Header { path, element });
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, headers)
}

/// The banners found in a text, in order.
fn banners_in(text: &str) -> Vec<usize> {
    text.lines()
        .filter_map(|line| line.strip_prefix("# banner ")?.parse().ok())
        .collect()
}

/// A comment paragraph that stands apart above a header is never the price
/// of removing something: not when a whole section empties (`payload`), not
/// when a list loses all its elements, not when a map or an optional table
/// goes. The one exception is by design: a removed list element goes as a
/// block, with the banners *inside* it — those above its own sub-tables.
#[test]
fn banners_outlive_whatever_is_removed_below_them() {
    type Removal = (&'static str, fn(&mut Config), &'static [&'static str]);
    // The edit, and the lists whose elements it removes.
    let removals: [Removal; 9] = [
        (
            "payload",
            |c| c.payload = Default::default(),
            &["payload.default", "payload.override", "payload.filter"],
        ),
        ("providers", |c| c.providers.clear(), &["providers"]),
        ("aliases", |c| c.aliases.clear(), &["aliases"]),
        ("pricing", |c| c.pricing.clear(), &["pricing"]),
        ("client keys", |c| c.auth.keys.clear(), &["auth.keys"]),
        (
            "models",
            |c| c.providers.iter_mut().for_each(|p| p.models.clear()),
            &["providers.models"],
        ),
        (
            "credentials",
            |c| c.providers.iter_mut().for_each(|p| p.credentials.clear()),
            &["providers.credentials"],
        ),
        (
            "headers",
            |c| c.providers.iter_mut().for_each(|p| p.headers.clear()),
            &[],
        ),
        ("tls", |c| c.server.tls = None, &[]),
    ];

    let mut rng = Rng::new(0xBA22E5);
    let mut exercised = [0usize; 9];
    let mut inside_blocks = 0;
    let mut one_provider = 0;
    for _ in 0..120 {
        let current = valid_config(&mut rng);
        let (text, headers) = with_banners(&pretty(&current));
        assert_eq!(parse(&text), current);
        let all: Vec<usize> = (0..headers.len()).collect();
        assert_eq!(banners_in(&text), all);

        let check = |next: &Config, gone: &dyn Fn(usize) -> bool, what: &str| -> usize {
            let rendered = render_update(&text, next).unwrap();
            assert_eq!(rendered.strategy, Strategy::Merged, "{what}");
            let out = rendered.text;
            assert_eq!(&parse(&out), next, "{what}");
            let expected: Vec<usize> = all.iter().copied().filter(|&n| !gone(n)).collect();
            assert_eq!(
                banners_in(&out),
                expected,
                "{what}\n--- before\n{text}\n--- after\n{out}"
            );
            let again = render_update(&out, next).unwrap();
            assert_eq!(again.strategy, Strategy::Unchanged, "{what}");
            all.len() - expected.len()
        };

        for (i, (what, removal, lists)) in removals.iter().enumerate() {
            let mut next = current.clone();
            removal(&mut next);
            if next == current || !next.validate().is_empty() {
                continue;
            }
            // Only what stands inside the block of a removed element goes:
            // the banners above that element's own sub-tables.
            let gone = |n: usize| {
                lists.iter().any(|list| {
                    headers[n]
                        .path
                        .strip_prefix(list)
                        .is_some_and(|rest| rest.starts_with('.'))
                })
            };
            inside_blocks += check(&next, &gone, what);
            exercised[i] += 1;
        }

        // One provider out of several: its block goes, its banner and the
        // blocks of the others stay.
        if !current.providers.is_empty() {
            let victim = rng.below(current.providers.len());
            let mut next = current.clone();
            next.providers.remove(victim);
            if next.validate().is_empty() {
                let mut provider = None;
                let block_of: Vec<Option<usize>> = headers
                    .iter()
                    .map(|header| {
                        if header.path == "providers" && header.element {
                            provider = Some(provider.map_or(0, |p| p + 1));
                            None
                        } else if header.path.starts_with("providers.") {
                            provider
                        } else {
                            None
                        }
                    })
                    .collect();
                let gone = |n: usize| block_of[n] == Some(victim);
                inside_blocks += check(&next, &gone, "one provider");
                one_provider += 1;
            }
        }
    }
    // Every removal was exercised, and so was the exception.
    assert!(exercised.iter().all(|&n| n > 10), "{exercised:?}");
    assert!(one_provider > 50, "{one_provider}");
    assert!(inside_blocks > 50, "{inside_blocks}");
}
