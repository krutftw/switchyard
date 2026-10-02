//! The route table of the client API (`docs/DESIGN.md` §9), as a pure
//! function from method and path to what should happen.
//!
//! Matching is done by hand rather than with axum's path patterns because
//! the Gemini routes do not fit them: `/v1beta/models/{model}:{method}`
//! where the model may itself contain `/` (prefixed models) and the method
//! is whatever follows the *last* `:`. Doing all of it in one place also
//! lets every unknown path and every wrong method be answered in the error
//! shape of the API family the path belongs to.

use http::Method;
use percent_encoding::percent_decode_str;
use switchyard_core::Protocol;

/// What a Gemini `:{method}` suffix asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GeminiMethod {
    /// `:generateContent`
    Generate,
    /// `:streamGenerateContent`
    Stream,
    /// `:countTokens`
    Count,
}

impl GeminiMethod {
    fn parse(method: &str) -> Option<GeminiMethod> {
        match method {
            "generateContent" => Some(GeminiMethod::Generate),
            "streamGenerateContent" => Some(GeminiMethod::Stream),
            "countTokens" => Some(GeminiMethod::Count),
            _ => None,
        }
    }

    /// The method as it is written in the URL.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            GeminiMethod::Generate => "generateContent",
            GeminiMethod::Stream => "streamGenerateContent",
            GeminiMethod::Count => "countTokens",
        }
    }
}

/// An OpenAI-style side endpoint that is proxied as raw JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RawEndpoint {
    Embeddings,
    ImageGenerations,
    Moderations,
    AudioSpeech,
}

impl RawEndpoint {
    /// The path relative to the provider's API root (the part after `/v1/`).
    pub(crate) const fn upstream_path(self) -> &'static str {
        match self {
            RawEndpoint::Embeddings => "embeddings",
            RawEndpoint::ImageGenerations => "images/generations",
            RawEndpoint::Moderations => "moderations",
            RawEndpoint::AudioSpeech => "audio/speech",
        }
    }

    /// The label of the request record.
    pub(crate) const fn endpoint(self) -> &'static str {
        match self {
            RawEndpoint::Embeddings => "POST /v1/embeddings",
            RawEndpoint::ImageGenerations => "POST /v1/images/generations",
            RawEndpoint::Moderations => "POST /v1/moderations",
            RawEndpoint::AudioSpeech => "POST /v1/audio/speech",
        }
    }
}

/// A route of the client API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// `GET /`
    Banner,
    /// `GET`, `HEAD /healthz`
    Health,
    /// `GET /v1/models`
    Models,
    /// `GET /v1/models/{id}`
    Model(String),
    /// `POST /v1/chat/completions`
    ChatCompletions,
    /// `POST /v1/completions`
    Completions,
    /// `POST /v1/responses`
    Responses,
    /// `GET /v1/responses` (WebSocket)
    ResponsesWebSocket,
    /// `POST /v1/responses/input_tokens`
    ResponsesInputTokens,
    /// `POST /v1/messages`
    Messages,
    /// `POST /v1/messages/count_tokens`
    MessagesCountTokens,
    /// `GET /v1beta/models`
    GeminiModels,
    /// `GET /v1beta/models/{name}`
    GeminiModel(String),
    /// `POST /v1beta/models/{model}:{method}` and the Vertex-style
    /// `POST /v1/models/{model}:{method}`.
    GeminiAction {
        model: String,
        method: GeminiMethod,
        /// The route prefix the request came in on, for the record label.
        prefix: &'static str,
    },
    /// `GET /v1/realtime` (WebSocket)
    Realtime,
    /// `POST /v1/embeddings` and friends.
    Raw(RawEndpoint),
}

impl Route {
    /// Whether the route is served without authentication.
    pub(crate) fn is_public(&self) -> bool {
        matches!(self, Route::Banner | Route::Health)
    }

    /// Whether the route only shows what the gateway serves — the model
    /// listings — rather than running a request (or opening a socket) on
    /// the client's behalf.
    pub(crate) fn is_listing(&self) -> bool {
        matches!(
            self,
            Route::Models | Route::Model(_) | Route::GeminiModels | Route::GeminiModel(_)
        )
    }
}

/// The outcome of matching a request against the route table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// The request matches `route`; errors are rendered for `protocol`.
    Route { route: Route, protocol: Protocol },
    /// No such path.
    NotFound { protocol: Protocol },
    /// The path exists, but not with this method.
    MethodNotAllowed {
        protocol: Protocol,
        /// Value of the `Allow` header.
        allow: &'static str,
    },
}

/// The generic OpenAI error shape, used wherever a path does not belong to
/// another vendor's API.
const OPENAI: Protocol = Protocol::OpenaiChat;

/// The protocol whose error envelope a path is answered in: Google's under
/// `/v1beta`, Anthropic's under `/v1/messages`, OpenAI's everywhere else.
pub(crate) fn protocol_of(path: &str) -> Protocol {
    if path == "/v1beta" || path.starts_with("/v1beta/") {
        Protocol::Gemini
    } else if path == "/v1/messages" || path.starts_with("/v1/messages/") {
        Protocol::Anthropic
    } else if path == "/v1/responses" || path.starts_with("/v1/responses/") {
        Protocol::OpenaiResponses
    } else {
        OPENAI
    }
}

fn found(route: Route, protocol: Protocol) -> Resolved {
    Resolved::Route { route, protocol }
}

/// A path that exists for exactly one method.
fn single(
    method: &Method,
    wanted: Method,
    allow: &'static str,
    route: Route,
    protocol: Protocol,
) -> Resolved {
    if *method == wanted {
        found(route, protocol)
    } else {
        Resolved::MethodNotAllowed { protocol, allow }
    }
}

/// Percent-decodes the variable part of a path. `None` when it is not UTF-8.
fn decode(raw: &str) -> Option<String> {
    percent_decode_str(raw)
        .decode_utf8()
        .ok()
        .map(|text| text.into_owned())
}

/// Splits `{model}:{method}` at the last `:`. The method must be in the
/// last path segment (a `:` further left belongs to the model name) and the
/// model must not be empty.
fn split_action(rest: &str) -> Option<(&str, &str)> {
    let (model, method) = rest.rsplit_once(':')?;
    if model.is_empty() || method.contains('/') {
        return None;
    }
    Some((model, method))
}

/// Matches a request against the route table. `path` is the raw request
/// path (no query string).
pub(crate) fn resolve(method: &Method, path: &str) -> Resolved {
    let protocol = protocol_of(path);
    let post = |route: Route| single(method, Method::POST, "POST", route, protocol);
    let get = |route: Route| single(method, Method::GET, "GET", route, protocol);
    match path {
        "/" => get(Route::Banner),
        "/healthz" => {
            if *method == Method::GET || *method == Method::HEAD {
                found(Route::Health, protocol)
            } else {
                Resolved::MethodNotAllowed {
                    protocol,
                    allow: "GET, HEAD",
                }
            }
        }
        "/v1/models" => get(Route::Models),
        "/v1/chat/completions" => post(Route::ChatCompletions),
        "/v1/completions" => post(Route::Completions),
        "/v1/responses" => {
            if *method == Method::POST {
                found(Route::Responses, protocol)
            } else if *method == Method::GET {
                found(Route::ResponsesWebSocket, protocol)
            } else {
                Resolved::MethodNotAllowed {
                    protocol,
                    allow: "GET, POST",
                }
            }
        }
        "/v1/responses/input_tokens" => post(Route::ResponsesInputTokens),
        "/v1/messages" => post(Route::Messages),
        "/v1/messages/count_tokens" => post(Route::MessagesCountTokens),
        "/v1/realtime" => get(Route::Realtime),
        "/v1/embeddings" => post(Route::Raw(RawEndpoint::Embeddings)),
        "/v1/images/generations" => post(Route::Raw(RawEndpoint::ImageGenerations)),
        "/v1/moderations" => post(Route::Raw(RawEndpoint::Moderations)),
        "/v1/audio/speech" => post(Route::Raw(RawEndpoint::AudioSpeech)),
        "/v1beta/models" => get(Route::GeminiModels),
        _ => {
            if let Some(rest) = path.strip_prefix("/v1beta/models/") {
                gemini_item(method, rest)
            } else if let Some(rest) = path.strip_prefix("/v1/models/") {
                openai_model_item(method, rest)
            } else {
                Resolved::NotFound { protocol }
            }
        }
    }
}

/// `/v1beta/models/{rest}`: a model lookup (`GET`) or an action (`POST`).
fn gemini_item(method: &Method, rest: &str) -> Resolved {
    let protocol = Protocol::Gemini;
    let Some(rest) = decode(rest).filter(|rest| !rest.is_empty()) else {
        return Resolved::NotFound { protocol };
    };
    if *method == Method::GET {
        return found(Route::GeminiModel(rest), protocol);
    }
    if *method != Method::POST {
        return Resolved::MethodNotAllowed {
            protocol,
            allow: "GET, POST",
        };
    }
    match split_action(&rest).and_then(|(model, name)| Some((model, GeminiMethod::parse(name)?))) {
        Some((model, method)) => found(
            Route::GeminiAction {
                model: model.to_string(),
                method,
                prefix: "/v1beta",
            },
            protocol,
        ),
        // No `:method`, or one this gateway does not serve.
        None => Resolved::NotFound { protocol },
    }
}

/// `/v1/models/{rest}`: an OpenAI (or Anthropic) model lookup, or — for
/// Vertex-style clients — a Gemini action when a `POST` names one.
fn openai_model_item(method: &Method, rest: &str) -> Resolved {
    let Some(rest) = decode(rest).filter(|rest| !rest.is_empty()) else {
        return Resolved::NotFound { protocol: OPENAI };
    };
    if *method == Method::GET {
        return found(Route::Model(rest), OPENAI);
    }
    let last_segment = rest.rsplit('/').next().unwrap_or(&rest);
    if *method == Method::POST && last_segment.contains(':') {
        // From here on the client is speaking Gemini.
        let protocol = Protocol::Gemini;
        return match split_action(&rest)
            .and_then(|(model, name)| Some((model, GeminiMethod::parse(name)?)))
        {
            Some((model, method)) => found(
                Route::GeminiAction {
                    model: model.to_string(),
                    method,
                    prefix: "/v1",
                },
                protocol,
            ),
            None => Resolved::NotFound { protocol },
        };
    }
    Resolved::MethodNotAllowed {
        protocol: OPENAI,
        allow: "GET",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn route(method: Method, path: &str) -> Route {
        match resolve(&method, path) {
            Resolved::Route { route, .. } => route,
            other => panic!("{method} {path} did not resolve to a route: {other:?}"),
        }
    }

    #[test]
    fn the_route_table() {
        for (method, path, expected) in [
            (Method::GET, "/", Route::Banner),
            (Method::GET, "/healthz", Route::Health),
            (Method::HEAD, "/healthz", Route::Health),
            (Method::GET, "/v1/models", Route::Models),
            (Method::POST, "/v1/chat/completions", Route::ChatCompletions),
            (Method::POST, "/v1/completions", Route::Completions),
            (Method::POST, "/v1/responses", Route::Responses),
            (Method::GET, "/v1/responses", Route::ResponsesWebSocket),
            (
                Method::POST,
                "/v1/responses/input_tokens",
                Route::ResponsesInputTokens,
            ),
            (Method::POST, "/v1/messages", Route::Messages),
            (
                Method::POST,
                "/v1/messages/count_tokens",
                Route::MessagesCountTokens,
            ),
            (Method::GET, "/v1beta/models", Route::GeminiModels),
            (Method::GET, "/v1/realtime", Route::Realtime),
            (
                Method::POST,
                "/v1/embeddings",
                Route::Raw(RawEndpoint::Embeddings),
            ),
            (
                Method::POST,
                "/v1/images/generations",
                Route::Raw(RawEndpoint::ImageGenerations),
            ),
            (
                Method::POST,
                "/v1/moderations",
                Route::Raw(RawEndpoint::Moderations),
            ),
            (
                Method::POST,
                "/v1/audio/speech",
                Route::Raw(RawEndpoint::AudioSpeech),
            ),
        ] {
            assert_eq!(route(method, path), expected, "{path}");
        }
    }

    #[test]
    fn model_ids_may_contain_slashes_and_escapes() {
        assert_eq!(
            route(Method::GET, "/v1/models/team/gpt-5"),
            Route::Model("team/gpt-5".into())
        );
        assert_eq!(
            route(Method::GET, "/v1/models/team%2Fgpt-5"),
            Route::Model("team/gpt-5".into())
        );
        assert_eq!(
            route(Method::GET, "/v1beta/models/models/gemini-pro"),
            Route::GeminiModel("models/gemini-pro".into())
        );
        // Not UTF-8 once decoded.
        assert_eq!(
            resolve(&Method::GET, "/v1/models/%ff"),
            Resolved::NotFound {
                protocol: Protocol::OpenaiChat
            }
        );
    }

    #[test]
    fn gemini_actions_split_at_the_last_colon() {
        assert_eq!(
            route(Method::POST, "/v1beta/models/gemini-pro:generateContent"),
            Route::GeminiAction {
                model: "gemini-pro".into(),
                method: GeminiMethod::Generate,
                prefix: "/v1beta",
            }
        );
        assert_eq!(
            route(
                Method::POST,
                "/v1beta/models/team/ft:gemini:v2:streamGenerateContent"
            ),
            Route::GeminiAction {
                model: "team/ft:gemini:v2".into(),
                method: GeminiMethod::Stream,
                prefix: "/v1beta",
            }
        );
        assert_eq!(
            route(Method::POST, "/v1beta/models/gemini-pro%3AcountTokens"),
            Route::GeminiAction {
                model: "gemini-pro".into(),
                method: GeminiMethod::Count,
                prefix: "/v1beta",
            }
        );
        for path in [
            "/v1beta/models/gemini-pro:embedContent",
            "/v1beta/models/gemini-pro",
            "/v1beta/models/:generateContent",
            "/v1beta/models/a:b/c",
            "/v1beta/models/",
        ] {
            assert_eq!(
                resolve(&Method::POST, path),
                Resolved::NotFound {
                    protocol: Protocol::Gemini
                },
                "{path}"
            );
        }
    }

    #[test]
    fn vertex_style_actions_need_a_post_and_a_colon() {
        assert_eq!(
            route(Method::POST, "/v1/models/team/gemini-pro:generateContent"),
            Route::GeminiAction {
                model: "team/gemini-pro".into(),
                method: GeminiMethod::Generate,
                prefix: "/v1",
            }
        );
        // An unknown method is a Gemini client's mistake.
        assert_eq!(
            resolve(&Method::POST, "/v1/models/gemini-pro:predict"),
            Resolved::NotFound {
                protocol: Protocol::Gemini
            }
        );
        // The colon has to be in the last segment.
        assert_eq!(
            resolve(&Method::POST, "/v1/models/a:b/c"),
            Resolved::MethodNotAllowed {
                protocol: Protocol::OpenaiChat,
                allow: "GET"
            }
        );
        assert_eq!(
            resolve(&Method::POST, "/v1/models/gpt-5"),
            Resolved::MethodNotAllowed {
                protocol: Protocol::OpenaiChat,
                allow: "GET"
            }
        );
        // A GET is a model lookup whatever the id looks like.
        assert_eq!(
            route(Method::GET, "/v1/models/gemini-pro:generateContent"),
            Route::Model("gemini-pro:generateContent".into())
        );
    }

    #[test]
    fn wrong_methods_and_unknown_paths() {
        for (method, path, protocol, allow) in [
            (Method::POST, "/", Protocol::OpenaiChat, "GET"),
            (Method::HEAD, "/", Protocol::OpenaiChat, "GET"),
            (Method::POST, "/healthz", Protocol::OpenaiChat, "GET, HEAD"),
            (
                Method::GET,
                "/v1/chat/completions",
                Protocol::OpenaiChat,
                "POST",
            ),
            (
                Method::DELETE,
                "/v1/responses",
                Protocol::OpenaiResponses,
                "GET, POST",
            ),
            (Method::GET, "/v1/messages", Protocol::Anthropic, "POST"),
            (Method::PUT, "/v1beta/models", Protocol::Gemini, "GET"),
            (
                Method::PUT,
                "/v1beta/models/gemini-pro",
                Protocol::Gemini,
                "GET, POST",
            ),
            (Method::POST, "/v1/realtime", Protocol::OpenaiChat, "GET"),
        ] {
            assert_eq!(
                resolve(&method, path),
                Resolved::MethodNotAllowed { protocol, allow },
                "{method} {path}"
            );
        }
        for (path, protocol) in [
            ("/nope", Protocol::OpenaiChat),
            ("/v1", Protocol::OpenaiChat),
            ("/v1/chat/completions/", Protocol::OpenaiChat),
            ("/v1/messages/batches", Protocol::Anthropic),
            ("/v1/responses/compact", Protocol::OpenaiResponses),
            ("/v1beta/files", Protocol::Gemini),
            ("/v1beta", Protocol::Gemini),
            ("/admin/api/status", Protocol::OpenaiChat),
        ] {
            assert_eq!(
                resolve(&Method::GET, path),
                Resolved::NotFound { protocol },
                "{path}"
            );
        }
    }

    #[test]
    fn only_the_banner_and_health_are_public() {
        assert!(Route::Banner.is_public());
        assert!(Route::Health.is_public());
        assert!(!Route::Models.is_public());
        assert!(!Route::Realtime.is_public());
    }
}
