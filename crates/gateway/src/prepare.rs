//! Building the upstream request of one attempt: passthrough or
//! translation (see `docs/DESIGN.md` section 2).

use crate::gateway::Inner;
use crate::summary::{SummaryAsk, strip_summary, summary_ask};
use crate::target::upstream_protocol;
use bytes::Bytes;
use http::HeaderMap;
use serde_json::Value;
use std::sync::Arc;
use switchyard_core::codec::{ClientCtx, RequestMeta, RequestPath, UpstreamCtx};
use switchyard_core::config::{Config, ProviderKind};
use switchyard_core::ir::Request;
use switchyard_core::{ApiError, Codec, Protocol, UpstreamError};
use switchyard_scheduler::{Lease, Resolved};
use switchyard_telemetry::Mode;
use switchyard_translate::{
    PayloadCtx, ReasoningInputs, ToolNames, apply_payload_rules, apply_to_body, apply_to_request,
    plan_with_label, sanitize_tool_names,
};
use switchyard_upstream::{Operation, Target, adapt_vertex_anthropic_body};
use tokio_util::sync::CancellationToken;

/// The client's request, as far as it stays the same across attempts.
pub(crate) struct Job {
    pub config: Arc<Config>,
    /// The client's codec.
    pub client: &'static dyn Codec,
    /// The client's body, parsed.
    pub body: Arc<Value>,
    /// Model (as the client wrote it) and stream flag.
    pub meta: RequestMeta,
    pub path_model: Option<String>,
    pub path_stream: Option<bool>,
    /// The client's request headers.
    pub headers: HeaderMap,
    /// Scope of per-client state (see `ClientIdentity::scope`).
    pub scope: String,
    /// The body carries a signature wrapped for another vendor, so it
    /// cannot be forwarded verbatim even to an upstream of its own protocol.
    pub wrapped: bool,
    /// The body decoded with the client's codec; filled on first use.
    pub decoded: Option<Request>,
    pub cancel: CancellationToken,
    /// The providers that refused this request a reasoning summary:
    /// translated Responses bodies for them are built without
    /// `reasoning.summary` from here on (see [`crate::summary`]). Starts
    /// empty.
    ///
    /// By provider: a provider of another organisation that the request
    /// fails over to is still asked. Kept here besides the gateway-wide
    /// memory because that one does not remember every refusal (a detail
    /// level one client chose is nobody else's affair) and nothing a
    /// request learns once the configuration has changed: the repeat is
    /// certain to leave the field out — and a request to be repeated once
    /// per provider at most — whatever is remembered for later requests.
    pub summary_refused_by: Vec<String>,
}

impl Job {
    /// The request decoded into the canonical model, decoding it on the
    /// first call. A body the client's codec cannot understand is the
    /// client's mistake.
    pub(crate) fn decoded(&mut self) -> Result<&Request, ApiError> {
        if self.decoded.is_none() {
            let path = RequestPath {
                model: self.path_model.as_deref(),
                stream: self.path_stream,
            };
            self.decoded = Some(self.client.decode_request(&self.body, &path)?);
        }
        match &self.decoded {
            Some(request) => Ok(request),
            // Unreachable: filled just above.
            None => Err(ApiError::internal("request could not be decoded")),
        }
    }

    /// Context for rendering output to the client: the model name exactly
    /// as the client wrote it, and the client's own request.
    pub(crate) fn client_ctx(&self) -> ClientCtx {
        ClientCtx::new(self.meta.model.clone()).with_request(Arc::clone(&self.body))
    }
}

/// An upstream request, ready to send.
pub(crate) struct Prepared {
    pub target: Target,
    /// Protocol of the upstream body.
    pub protocol: Protocol,
    pub mode: Mode,
    /// The serialised upstream body.
    pub body: Bytes,
    /// Tool names rewritten for the upstream (translation only).
    pub names: ToolNames,
    /// Label of the reasoning depth that goes upstream, for the record.
    pub reasoning: Option<&'static str>,
    /// The reasoning summary a translated Responses request asks for:
    /// should the upstream refuse exactly that, the attempt is worth
    /// repeating without it (see [`crate::summary`]). Always
    /// [`SummaryAsk::Nothing`] for passthrough and for a Responses client,
    /// where the field is the client's own.
    pub summary: SummaryAsk,
}

/// Why no upstream request could be built.
pub(crate) enum PrepareError {
    /// The client's request is at fault (or cannot be expressed for this
    /// upstream): answer with this error, do not try other credentials.
    Client(ApiError),
    /// The credential cannot be used: a failed attempt.
    Upstream(UpstreamError),
}

/// What the target model accepts, in the form the codecs want it.
pub(crate) fn upstream_ctx(lease: &Lease) -> UpstreamCtx<'_> {
    UpstreamCtx {
        thinking: lease.info.thinking_caps(),
        max_output_tokens: lease.info.max_output_tokens,
        quirks: lease.quirks,
    }
}

/// Applies the Vertex AI body conventions to a body built for `protocol`.
pub(crate) fn adapt_for_vertex(body: &mut Value, protocol: Protocol, op: &Operation) {
    match protocol {
        Protocol::Anthropic => adapt_vertex_anthropic_body(body, op),
        _ => switchyard_codecs::gemini::adapt_for_vertex(body),
    }
}

impl Inner {
    /// Builds the upstream request for a generation attempt with `lease`.
    ///
    /// * **Passthrough** — the upstream speaks the client's protocol and the
    ///   body carries no foreign signature: the client's JSON with the model
    ///   replaced, reasoning rewritten when a suffix (or the model's
    ///   capabilities) demands it, payload rules applied, and the codec's
    ///   `prepare_passthrough` run last.
    /// * **Translation** — otherwise: the decoded request with the model and
    ///   stream flag set, reasoning fitted, tool names made valid for the
    ///   upstream, remembered reasoning restored, encoded by the upstream's
    ///   codec, then payload rules. A Responses body for a provider that
    ///   refuses reasoning summaries is built without `reasoning.summary`.
    pub(crate) async fn prepare_generate(
        &self,
        job: &mut Job,
        resolved: &Resolved,
        lease: &Lease,
    ) -> Result<Prepared, PrepareError> {
        let protocol = upstream_protocol(lease);
        let upstream = switchyard_codecs::codec(protocol);
        let stream = job.meta.stream;
        let op = Operation::Generate { stream };
        let ctx = upstream_ctx(lease);
        let suffix = resolved.depth_for(lease);
        let passthrough = job.client.protocol() == protocol && !job.wrapped;
        let rules = PayloadCtx {
            upstream_model: &lease.upstream_model,
            requested_model: &resolved.base,
            protocol,
            provider: &lease.credential.provider,
            same_protocol: passthrough,
        };

        let mut summary = SummaryAsk::Nothing;
        let (mut body, names, reasoning) = if passthrough {
            let mut body = (*job.body).clone();
            upstream.set_request_model(&mut body, &lease.upstream_model);
            let asked = job.client.read_reasoning(&job.body);
            let (plan, label) = plan_with_label(&ReasoningInputs {
                client: &asked,
                suffix,
                model: ctx.thinking,
                target: protocol,
                passthrough: true,
            });
            apply_to_body(upstream, &mut body, plan, &ctx);
            apply_payload_rules(&job.config.payload, &rules, Some(&job.body), &mut body);
            // Last, and always: codecs repair what a forwarded body may not
            // contain (foreign signatures, omissions) and rely on the model
            // already being the upstream's.
            upstream.prepare_passthrough(&mut body, stream, &ctx);
            (body, ToolNames::new(), label)
        } else {
            let mut request = job.decoded().map_err(PrepareError::Client)?.clone();
            request.model = lease.upstream_model.clone();
            request.stream = stream;
            let asked = request.reasoning.clone().unwrap_or_default();
            let (plan, label) = plan_with_label(&ReasoningInputs {
                client: &asked,
                suffix,
                model: ctx.thinking,
                target: protocol,
                passthrough: false,
            });
            apply_to_request(&mut request, plan);
            let names = sanitize_tool_names(&mut request, protocol);
            self.reasoning.restore(&mut request, protocol, &job.scope);
            let mut body = upstream
                .encode_request(&request, &ctx)
                .map_err(|error| PrepareError::Client(ApiError::from(error)))?;
            apply_payload_rules(&job.config.payload, &rules, None, &mut body);
            // A Responses client's body is re-encoded too when it carries
            // another vendor's signature; its `reasoning.summary` is its
            // own and stays, as in passthrough.
            let gateway_asks = job.client.protocol() != Protocol::OpenaiResponses;
            if protocol == Protocol::OpenaiResponses && gateway_asks {
                // The summary request is the gateway's doing, not the
                // client's: an upstream known to refuse it — the provider,
                // or this model of it — is not asked (after the payload
                // rules, which cannot make it accept).
                let provider = &lease.credential.provider;
                if job.summary_refused_by.contains(provider)
                    || self
                        .summary_refusals
                        .covers(&job.config, provider, &lease.upstream_model)
                {
                    strip_summary(&mut body);
                } else {
                    summary = summary_ask(&body);
                }
            }
            (body, names, label)
        };

        if lease.credential.kind == ProviderKind::Vertex {
            adapt_for_vertex(&mut body, protocol, &op);
        }
        let target = self
            .target_for(
                &job.config,
                &lease.provider_config,
                &lease.credential,
                protocol,
                &lease.upstream_model,
            )
            .await
            .map_err(PrepareError::Upstream)?;
        let body = serde_json::to_vec(&body)
            .map(Bytes::from)
            .map_err(|error| {
                PrepareError::Client(ApiError::internal(format!(
                    "the upstream request could not be serialised: {error}"
                )))
            })?;
        Ok(Prepared {
            target,
            protocol,
            mode: if passthrough {
                Mode::Passthrough
            } else {
                Mode::Translated
            },
            body,
            names,
            reasoning,
            summary,
        })
    }
}
