//! Many providers: building, rebuilding and reading the scheduler must take
//! time proportional to the size of the configuration.
//!
//! With some 300 providers every edit made in the dashboard took seconds:
//! each applied configuration rebuilt the registry once and then once more
//! per mock provider (the gateway hands every mock provider its model list
//! with `set_discovered`), and the model table compared every route of an
//! alias with every other one. The tests compare a small fleet with one ten
//! times its size: linear work costs about ten times as much, quadratic work
//! a hundred times.

mod common;

use common::resolver;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use switchyard_core::ModelInfo;
use switchyard_core::config::{AliasConfig, Config, ProviderConfig, ProviderKind};
use switchyard_scheduler::Scheduler;

/// The models every mock provider serves.
const MODELS: [&str; 8] = [
    "mock-echo",
    "mock-lorem",
    "mock-think",
    "mock-tools",
    "mock-slow",
    "mock-error-429",
    "mock-error-500",
    "mock-error-401",
];

const ALIASES: usize = 50;
const SMALL: usize = 30;
const LARGE: usize = 300;
/// How much longer the large fleet may take than the small one. Ten times
/// the providers is ten times the work when it is linear and a hundred times
/// when it is quadratic; the bound sits between the two with room for the
/// noise of a busy machine. (Measured: 7 to 10 in debug and release builds;
/// before the fixes, 108 for `apply` and 32 for `views`.)
const MAX_RATIO: f64 = 20.0;
/// No single operation on the large fleet may take longer than this, even
/// in an unoptimised build on a slow machine. (Measured: 60 ms at most in
/// debug builds, 14 ms in release builds, where building or rebuilding
/// takes 5 to 8 ms.)
const MAX_TIME: Duration = Duration::from_secs(2);

fn mock_models() -> Vec<ModelInfo> {
    MODELS
        .iter()
        .map(|id| ModelInfo {
            id: (*id).to_string(),
            owned_by: Some("switchyard".to_string()),
            context_window: Some(128_000),
            max_output_tokens: Some(8_192),
            known: true,
            ..ModelInfo::default()
        })
        .collect()
}

/// `providers` mock providers of eight models each — every fourth one under
/// a prefix, in three priority tiers — and fifty aliases over them.
fn fleet(providers: usize) -> (Config, HashMap<String, Vec<ModelInfo>>) {
    let mut config = Config::default();
    let mut lists = HashMap::new();
    for index in 0..providers {
        let mut provider = ProviderConfig::new(format!("mock-{index:03}"), ProviderKind::Mock);
        provider.priority = (index % 3) as i32;
        if index % 4 == 0 {
            provider.prefix = format!("team{index}");
        }
        lists.insert(provider.name.clone(), mock_models());
        config.providers.push(provider);
    }
    for index in 0..ALIASES {
        config.aliases.push(AliasConfig {
            name: format!("alias-{index:02}"),
            targets: vec![
                MODELS[index % MODELS.len()].to_string(),
                format!("{}(high)", MODELS[(index + 1) % MODELS.len()]),
                format!("team0/{}", MODELS[(index + 2) % MODELS.len()]),
            ],
            hide_targets: index % 10 == 0,
        });
    }
    let issues = config.validate();
    assert!(issues.is_empty(), "{issues:?}");
    (config, lists)
}

/// The shortest of `runs` runs: the one least disturbed by whatever else
/// the machine was doing.
fn best_of(runs: usize, mut work: impl FnMut()) -> Duration {
    (0..runs)
        .map(|_| {
            let started = Instant::now();
            work();
            started.elapsed()
        })
        .min()
        .unwrap_or(Duration::ZERO)
}

/// Time of each operation for a fleet of `providers`.
struct Timings {
    /// A new scheduler that knows every provider's model list.
    build: Duration,
    /// Applying the same configuration again (lists are remembered).
    rebuild: Duration,
    /// What the gateway does for every applied configuration: a rebuild,
    /// then each mock provider's list handed over once more.
    apply: Duration,
    /// The admin API's views: providers, model table, listing.
    views: Duration,
}

fn measure(providers: usize, runs: usize) -> Timings {
    let (config, lists) = fleet(providers);
    let build = best_of(runs, || {
        let scheduler = Scheduler::new(&config, &resolver);
        assert_eq!(scheduler.set_discovered_many(lists.clone()), providers);
        // Every model by its bare name and once per prefix, and the aliases.
        assert_eq!(
            scheduler.models_routable(),
            MODELS.len() * (1 + providers.div_ceil(4)) + ALIASES
        );
    });

    let scheduler = Scheduler::new(&config, &resolver);
    scheduler.rebuild(&config, &resolver, lists.clone());
    let rebuild = best_of(runs, || {
        scheduler.rebuild(&config, &resolver, HashMap::new());
    });
    let apply = best_of(runs, || {
        scheduler.rebuild(&config, &resolver, HashMap::new());
        for provider in &config.providers {
            assert!(scheduler.set_discovered(&provider.name, mock_models()));
        }
    });
    let views = best_of(runs, || {
        let snapshot = scheduler.snapshot();
        let models = scheduler.models();
        let listed = scheduler.visible_models();
        assert_eq!(snapshot.len(), providers);
        assert!(models.len() >= MODELS.len() + ALIASES);
        assert!(listed.len() >= ALIASES);
    });

    // The table is what it should be, whatever its size.
    let models = scheduler.models();
    let echo = models
        .iter()
        .find(|entry| entry.name == "mock-echo")
        .expect("mock-echo is served");
    assert_eq!(echo.routes.len(), providers);
    let alias = models
        .iter()
        .find(|entry| entry.name == "alias-00")
        .expect("alias-00 is listed");
    // Two targets served by every provider and one by the first only.
    assert_eq!(alias.routes.len(), 2 * providers + 1);

    Timings {
        build,
        rebuild,
        apply,
        views,
    }
}

fn ratio(large: Duration, small: Duration) -> f64 {
    large.as_secs_f64() / small.as_secs_f64().max(1e-9)
}

#[test]
fn work_grows_with_the_number_of_providers_not_with_its_square() {
    // The small fleet is measured more often: its times are short, so the
    // best run needs more tries to be free of noise.
    let small = measure(SMALL, 12);
    let large = measure(LARGE, 4);
    let rows = [
        ("build", small.build, large.build),
        ("rebuild", small.rebuild, large.rebuild),
        ("apply", small.apply, large.apply),
        ("views", small.views, large.views),
    ];
    for (name, small, large) in rows {
        println!(
            "{name:8} {SMALL:3} providers {small:>12.3?}   {LARGE} providers {large:>12.3?}   x{:.1}",
            ratio(large, small)
        );
    }
    for (name, small, large) in rows {
        let ratio = ratio(large, small);
        assert!(
            ratio <= MAX_RATIO,
            "{name}: {LARGE} providers take {large:?}, {ratio:.1} times the {small:?} of {SMALL} \
             providers; ten times the providers should cost about ten times as much"
        );
        assert!(
            large <= MAX_TIME,
            "{name}: {LARGE} providers take {large:?}"
        );
    }
}
