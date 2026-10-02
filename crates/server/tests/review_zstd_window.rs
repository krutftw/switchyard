//! Review: valid zstd request bodies are refused as "not valid zstd data"
//! when `server.body_limit_mb` is small.
//!
//! `body.rs` caps the decoder's window (`window_log_max`) at the power of
//! two just above the body limit: 2 MiB for a 1 MiB limit. A zstd frame
//! written by a *streaming* encoder — one that does not know the size of
//! its input up front — declares the window of its compression level, not
//! of its content: 4 MiB from level 10, 8 MiB from level 17 (and 8 MiB by
//! default in Go's widely used `klauspost/compress` writer). Such a frame is
//! refused whatever it contains, with a 400 that blames the client's data,
//! although the body is tiny and perfectly valid.
//!
//! The requirement is a cap on the *decompressed size* ("decoded with a
//! decompressed-size cap equal to the same limit"), which these bodies are
//! far below.

mod support;

use serde_json::{Value, json};
use std::io::Write;
use support::{KEY, Settings, TestServer, http};

/// What a streaming encoder produces: the frame header is written before
/// the encoder knows how little input there is.
fn zstd_streamed(data: &[u8], level: i32) -> Vec<u8> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

#[tokio::test]
async fn a_small_zstd_body_from_a_streaming_encoder_is_decoded_whatever_its_level() {
    // The limit the other tests of this crate run with.
    let server = TestServer::with(Settings {
        body_limit_mb: 1,
        ..Settings::default()
    })
    .await;
    let body = json!({"model": "mock-echo",
                      "messages": [{"role": "user", "content": "hello zstd"}]})
    .to_string();

    for level in [3, 10, 19] {
        let compressed = zstd_streamed(body.as_bytes(), level);
        // Sanity: this is valid zstd, a few hundred bytes of it.
        assert_eq!(
            zstd::stream::decode_all(&compressed[..]).unwrap(),
            body.as_bytes()
        );
        assert!(compressed.len() < 1024);

        let response = http()
            .post(server.url("/v1/chat/completions"))
            .bearer_auth(KEY)
            .header("content-type", "application/json")
            .header("content-encoding", "zstd")
            .body(compressed)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let answer: Value = response.json().await.unwrap();
        assert_eq!(status, 200, "level {level}: {answer}");
        assert_eq!(
            answer["choices"][0]["message"]["content"], "hello zstd",
            "level {level}"
        );
    }
}
