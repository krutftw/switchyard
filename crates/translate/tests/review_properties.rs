//! Randomised model checks written during the review. They all PASS against
//! the reviewed implementation; they are kept as guard rails for the fixes to
//! the findings in `review_transcode.rs` and `review_reasoning_store.rs`.
//! Everything is driven by a fixed-seed xorshift generator, so the runs are
//! deterministic.

use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use switchyard_core::ir::{Message, Part, Reasoning, Request, Response, Role, Signature};
use switchyard_core::protocol::Protocol;
use switchyard_translate::jsonpath;
use switchyard_translate::reasoning_store::{ManualClock, ReasoningStore};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn signed(text: &str) -> Part {
    Part::Reasoning(Reasoning {
        id: None,
        text: text.into(),
        signature: Some(Signature::new(Protocol::Anthropic, format!("sig-{text}"))),
        redacted: false,
    })
}

fn random_turn(rng: &mut Rng, round: usize) -> Vec<Part> {
    let len = 1 + rng.below(8) as usize;
    (0..len)
        .map(|i| match rng.below(3) {
            0 => signed(&format!("r{i}")),
            1 => Part::text(format!("t{i}")),
            _ => Part::tool_call(format!("c{round}-{i}"), "f", "{}"),
        })
        .collect()
}

fn store_with(parts: &[Part]) -> ReasoningStore {
    let store = ReasoningStore::new(64, Duration::from_secs(60));
    let mut response = Response::new("r", "m");
    response.parts = parts.to_vec();
    store.remember(&response, "k");
    store
}

fn without_reasoning(parts: &[Part]) -> Vec<Part> {
    parts
        .iter()
        .filter(|p| !matches!(p, Part::Reasoning(_)))
        .cloned()
        .collect()
}

/// `set` reports the truth (a successful write is readable, a failed one
/// changes nothing), `remove` removes exactly what `get` finds, and nothing
/// panics, for random paths over objects, arrays, nulls and scalars.
#[test]
fn jsonpath_set_get_remove_agree() {
    let mut rng = Rng(42);
    let atoms = [
        "a", "b", "0", "1", "2", "", "\\.", "\\", "x\\.y", "00", "-1",
    ];
    for _ in 0..50_000 {
        let mut root = match rng.below(6) {
            0 => json!({}),
            1 => json!([]),
            2 => json!({"a": [1, {"b": null}], "b": {"0": [[], 3]}, "": 1}),
            3 => json!([[1, 2], {"a": "s"}, null]),
            4 => Value::Null,
            _ => json!("scalar"),
        };
        for _ in 0..4 {
            let segments = rng.below(4) as usize;
            let path = (0..segments)
                .map(|_| atoms[rng.below(atoms.len() as u64) as usize])
                .collect::<Vec<_>>()
                .join(".");
            let before = root.clone();
            if rng.below(2) == 0 {
                if jsonpath::set(&mut root, &path, json!("V")) {
                    assert_eq!(jsonpath::get(&root, &path), Some(&json!("V")), "{path:?}");
                } else {
                    assert_eq!(root, before, "failed set changed the value: {path:?}");
                }
            } else {
                let existed = jsonpath::get(&root, &path).is_some();
                assert_eq!(jsonpath::remove(&mut root, &path), existed, "{path:?}");
                if !existed {
                    assert_eq!(root, before);
                }
            }
        }
    }
}

/// The store behaves like a reference LRU with a sliding TTL under random
/// remember / restore / forget / clock-advance operations.
#[test]
fn reasoning_store_matches_a_reference_lru() {
    const TTL: u64 = 100;
    const CAPACITY: usize = 5;
    let mut rng = Rng(7);
    let clock = Arc::new(ManualClock::new());
    let store = ReasoningStore::with_clock(CAPACITY, Duration::from_secs(TTL), clock.clone());
    // (id, last touched), most recently used first.
    let mut model: Vec<(String, u64)> = Vec::new();
    let mut now = 0u64;
    for step in 0..50_000 {
        let id = format!("id{}", rng.below(9));
        match rng.below(5) {
            0 | 1 => {
                let mut response = Response::new("r", "m");
                response.parts = vec![signed(&id), Part::tool_call(id.clone(), "f", "{}")];
                store.remember(&response, "k");
                model.retain(|(other, touched)| now - touched < TTL && other != &id);
                model.insert(0, (id, now));
                model.truncate(CAPACITY);
            }
            2 => {
                let mut request = Request::new("m", Protocol::OpenaiChat);
                request.messages = vec![Message::new(
                    Role::Assistant,
                    vec![Part::tool_call(id.clone(), "f", "{}")],
                )];
                let restored = store.restore(&mut request, Protocol::Anthropic, "k");
                let known = match model.iter().position(|(other, _)| other == &id) {
                    Some(at) => {
                        let live = now - model[at].1 < TTL;
                        model.remove(at);
                        if live {
                            model.insert(0, (id, now));
                        }
                        live
                    }
                    None => false,
                };
                assert_eq!(!restored.is_empty(), known, "step {step}");
            }
            3 => {
                let by = rng.below(60);
                now += by;
                clock.advance(Duration::from_secs(by));
            }
            _ => {
                let was_stored = model.iter().any(|(other, _)| other == &id);
                assert_eq!(store.forget_call("k", &id), was_stored, "step {step}");
                model.retain(|(other, _)| other != &id);
            }
        }
        if step % 97 == 0 {
            model.retain(|(_, touched)| now - touched < TTL);
            assert_eq!(store.len(), model.len(), "step {step}");
            assert!(store.len() <= CAPACITY);
        }
    }
}

/// When the client stripped only the reasoning, `restore` rebuilds the turn
/// exactly (reasoning after the last tool call is not remembered and stays
/// gone).
#[test]
fn restore_rebuilds_a_turn_that_lost_only_its_reasoning() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for round in 0..10_000 {
        let parts = random_turn(&mut rng, round);
        let Some(last_call) = parts.iter().rposition(|p| matches!(p, Part::ToolCall(_))) else {
            continue;
        };
        let store = store_with(&parts);
        let mut request = Request::new("m", Protocol::OpenaiChat);
        request.messages = vec![Message::new(Role::Assistant, without_reasoning(&parts))];
        store.restore(&mut request, Protocol::Anthropic, "k");
        let expected: Vec<Part> = parts
            .iter()
            .enumerate()
            .filter(|(at, part)| !(matches!(part, Part::Reasoning(_)) && *at > last_call))
            .map(|(_, part)| part.clone())
            .collect();
        assert_eq!(request.messages[0].parts, expected, "round {round}");
    }
}

/// Whatever the client did to the turn (dropped text or calls, kept some
/// signed reasoning, echoed some of it unsigned), `restore` never reorders or
/// drops the client's own non-reasoning parts, never inserts the same signed
/// part twice, and is idempotent.
#[test]
fn restore_never_duplicates_and_is_idempotent() {
    let mut rng = Rng(0xDEAD_BEEF);
    for round in 0..20_000 {
        let parts = random_turn(&mut rng, round);
        let store = store_with(&parts);
        let mut client = Vec::new();
        for part in &parts {
            match part {
                Part::Reasoning(reasoning) => match rng.below(4) {
                    0 => client.push(part.clone()),
                    1 => client.push(Part::reasoning(reasoning.text.clone())),
                    _ => {}
                },
                Part::Text(_) if rng.below(3) == 0 => {}
                Part::ToolCall(_) if rng.below(5) == 0 => {}
                _ => client.push(part.clone()),
            }
        }
        let mut request = Request::new("m", Protocol::OpenaiChat);
        request.messages = vec![Message::new(Role::Assistant, client.clone())];
        store.restore(&mut request, Protocol::Anthropic, "k");
        let after = request.messages[0].parts.clone();

        assert_eq!(
            without_reasoning(&after),
            without_reasoning(&client),
            "round {round}"
        );
        let signed_parts: Vec<&Part> = after
            .iter()
            .filter(|p| matches!(p, Part::Reasoning(r) if r.signature.is_some()))
            .collect();
        for (at, part) in signed_parts.iter().enumerate() {
            assert!(
                !signed_parts[at + 1..].contains(part),
                "round {round}: duplicated reasoning in {after:?}"
            );
        }
        let again = store.restore(&mut request, Protocol::Anthropic, "k");
        assert!(
            again.is_empty(),
            "round {round}: second pass changed the turn"
        );
        assert_eq!(request.messages[0].parts, after, "round {round}");
    }
}
