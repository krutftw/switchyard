//! Randomised checks of where `ReasoningStore::restore` puts reasoning back
//! when the client's version of a turn differs from the response it came
//! from (regression guard for review finding R-S1). Driven by a fixed-seed
//! xorshift generator, so every run is the same.

use std::time::Duration;
use switchyard_core::ir::{Message, Part, Reasoning, Request, Response, Role, Signature};
use switchyard_core::protocol::Protocol;
use switchyard_translate::reasoning_store::ReasoningStore;

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

/// A response turn of up to ten parts; every part is unique.
fn random_turn(rng: &mut Rng, round: usize) -> Vec<Part> {
    let len = 1 + rng.below(10) as usize;
    (0..len)
        .map(|i| match rng.below(3) {
            0 => signed(&format!("r{i}")),
            1 => Part::text(format!("t{i}")),
            _ => Part::tool_call(format!("c{round}-{i}"), "f", "{}"),
        })
        .collect()
}

fn is_call(part: &Part) -> bool {
    matches!(part, Part::ToolCall(_))
}

fn is_reasoning(part: &Part) -> bool {
    matches!(part, Part::Reasoning(_))
}

/// Remembers `parts` and then forgets the entries of some of its calls, as
/// eviction and expiry would. Returns the store and the calls still known.
fn store_with(parts: &[Part], rng: &mut Rng, forget_some: bool) -> (ReasoningStore, Vec<String>) {
    let store = ReasoningStore::new(64, Duration::from_secs(60));
    let mut response = Response::new("r", "m");
    response.parts = parts.to_vec();
    store.remember(&response, "k");
    let mut live = Vec::new();
    let mut reasoning_seen = false;
    for part in parts {
        match part {
            Part::Reasoning(_) => reasoning_seen = true,
            // Only calls with reasoning in front of them have an entry.
            Part::ToolCall(call) if reasoning_seen => {
                if forget_some && rng.below(3) == 0 {
                    assert!(store.forget_call("k", &call.id));
                } else {
                    live.push(call.id.clone());
                }
            }
            _ => {}
        }
    }
    (store, live)
}

fn restore(store: &ReasoningStore, client: Vec<Part>) -> Vec<Part> {
    let mut request = Request::new("m", Protocol::OpenaiChat);
    request.messages = vec![Message::new(Role::Assistant, client)];
    store.restore(&mut request, Protocol::Anthropic, "k");
    request.messages.remove(0).parts
}

/// The positions, in the response, of the calls and signed reasoning parts
/// of a restored turn. In a correct restore they only ever increase.
fn response_positions(response: &[Part], restored: &[Part]) -> Vec<usize> {
    restored
        .iter()
        .filter(|part| is_call(part) || matches!(part, Part::Reasoning(r) if r.signature.is_some()))
        .map(|part| {
            response
                .iter()
                .position(|original| original == part)
                .expect("restore only inserts parts of the response")
        })
        .collect()
}

fn assert_in_response_order(response: &[Part], restored: &[Part], context: &str) {
    let positions = response_positions(response, restored);
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{context}: reasoning and calls are out of order: {positions:?}\n response: {response:#?}\n restored: {restored:#?}"
    );
}

/// When the client only stripped the reasoning, the turn is rebuilt exactly,
/// even if some of its calls are no longer known: one surviving entry is
/// enough to bring back everything that preceded its call.
#[test]
fn a_stripped_turn_is_rebuilt_exactly_from_whatever_entries_survive() {
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    let mut partial = 0;
    for round in 0..20_000 {
        let parts = random_turn(&mut rng, round);
        let (store, live) = store_with(&parts, &mut rng, true);
        let last_live = parts
            .iter()
            .rposition(|part| matches!(part, Part::ToolCall(call) if live.contains(&call.id)));
        let stripped: Vec<Part> = parts.iter().filter(|p| !is_reasoning(p)).cloned().collect();
        let restored = restore(&store, stripped);

        let expected: Vec<Part> = parts
            .iter()
            .enumerate()
            .filter(|(at, part)| !is_reasoning(part) || last_live.is_some_and(|last| *at < last))
            .map(|(_, part)| part.clone())
            .collect();
        assert_eq!(
            restored, expected,
            "round {round}: {parts:#?}, live {live:?}"
        );
        if last_live.is_some() && live.len() < parts.iter().filter(|p| is_call(p)).count() {
            partial += 1;
        }
    }
    assert!(partial > 1_000, "the partial case was hardly exercised");
}

/// A Chat Completions client: no reasoning, all text merged into one string
/// in front of the calls, some calls dropped.
#[test]
fn a_chat_style_echo_keeps_reasoning_and_calls_in_response_order() {
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    let mut restored_some = 0;
    for round in 0..20_000 {
        let parts = random_turn(&mut rng, round);
        let forget_some = rng.below(2) == 0;
        let (store, _) = store_with(&parts, &mut rng, forget_some);

        let text: String = parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        let mut client = Vec::new();
        if !text.is_empty() {
            client.push(Part::text(text));
        }
        for part in &parts {
            if is_call(part) && rng.below(5) > 0 {
                client.push(part.clone());
            }
        }

        let restored = restore(&store, client.clone());
        assert_in_response_order(&parts, &restored, &format!("round {round}"));
        // The client's own parts are all still there, in their order.
        let kept: Vec<Part> = restored
            .iter()
            .filter(|p| !is_reasoning(p))
            .cloned()
            .collect();
        assert_eq!(kept, client, "round {round}");
        if restored.len() > client.len() {
            restored_some += 1;
            // A second pass has nothing left to do.
            assert_eq!(restore(&store, restored.clone()), restored, "round {round}");
        }
    }
    assert!(restored_some > 5_000);
}

/// A client that keeps the order of the turn but drops, keeps or echoes
/// parts at will, and splits or merges neighbouring text.
#[test]
fn an_edited_turn_keeps_reasoning_and_calls_in_response_order() {
    let mut rng = Rng(0xFEED_FACE_0BAD_F00D);
    for round in 0..40_000 {
        let parts = random_turn(&mut rng, round);
        let forget_some = rng.below(2) == 0;
        let (store, _) = store_with(&parts, &mut rng, forget_some);

        let mut client: Vec<Part> = Vec::new();
        for part in &parts {
            match part {
                Part::Reasoning(reasoning) => match rng.below(5) {
                    0 => client.push(part.clone()),
                    1 => client.push(Part::reasoning(reasoning.text.clone())),
                    _ => {}
                },
                Part::Text(text) => match rng.below(4) {
                    0 => {}
                    1 => {
                        // Split in two.
                        client.push(Part::text(format!("{}-a", text.text)));
                        client.push(Part::text(format!("{}-b", text.text)));
                    }
                    2 => match client.last_mut() {
                        // Merged into the text before it.
                        Some(Part::Text(previous)) => previous.text.push_str(&text.text),
                        _ => client.push(part.clone()),
                    },
                    _ => client.push(part.clone()),
                },
                _ if rng.below(5) == 0 => {}
                _ => client.push(part.clone()),
            }
        }

        let restored = restore(&store, client.clone());
        assert_in_response_order(&parts, &restored, &format!("round {round}"));
        let non_reasoning = |parts: &[Part]| -> Vec<Part> {
            parts.iter().filter(|p| !is_reasoning(p)).cloned().collect()
        };
        assert_eq!(
            non_reasoning(&restored),
            non_reasoning(&client),
            "round {round}"
        );
        assert_eq!(restore(&store, restored.clone()), restored, "round {round}");
    }
}
