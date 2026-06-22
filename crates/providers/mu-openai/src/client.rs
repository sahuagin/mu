use crate::{CreateResponseRequest, Response, ResponseStreamEvent, SseStream};
use futures::{Stream, StreamExt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    PublicApi {
        base: String,
    },
    Codex {
        url: String,
        chatgpt_account_id: String,
        originator: String,
    },
}
impl Endpoint {
    pub fn public_api() -> Self {
        Self::PublicApi {
            base: "https://api.openai.com/v1".into(),
        }
    }
    pub fn codex(chatgpt_account_id: impl Into<String>) -> Self {
        Self::Codex {
            url: "https://chatgpt.com/backend-api/codex/responses".into(),
            chatgpt_account_id: chatgpt_account_id.into(),
            originator: "mu".into(),
        }
    }
    fn responses_url(&self) -> String {
        match self {
            Endpoint::PublicApi { base } => format!("{}/responses", base.trim_end_matches('/')),
            Endpoint::Codex { url, .. } => url.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    Bearer(String),
}

pub struct Client {
    http: reqwest::Client,
    endpoint: Endpoint,
    auth: Auth,
}
impl Client {
    pub fn new(endpoint: Endpoint, auth: Auth) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint,
            auth,
        }
    }
    pub async fn create_response(
        &self,
        request: &CreateResponseRequest,
    ) -> Result<Response, ClientError> {
        let resp = self.send(request).await?;
        let status = resp.status();
        let text = resp.text().await.map_err(ClientError::Http)?;
        if !status.is_success() {
            return Err(ClientError::Status { status, body: text });
        }
        serde_json::from_str(&text).map_err(ClientError::Json)
    }
    pub async fn stream_response(
        &self,
        request: &CreateResponseRequest,
    ) -> Result<
        impl Stream<Item = Result<ResponseStreamEvent, ClientError>> + Send + 'static,
        ClientError,
    > {
        let resp = self.send(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Status { status, body });
        }
        let sse = SseStream::new(resp.bytes_stream());
        Ok(sse.filter_map(|ev| async move {
            match ev {
                Err(e) => Some(Err(ClientError::Sse(e))),
                Ok(e) if e.data.trim().is_empty() || e.data.trim() == "[DONE]" => None,
                Ok(e) => Some(
                    serde_json::from_str::<ResponseStreamEvent>(&e.data).map_err(ClientError::Json),
                ),
            }
        }))
    }
    async fn send(
        &self,
        request: &CreateResponseRequest,
    ) -> Result<reqwest::Response, ClientError> {
        let Auth::Bearer(token) = &self.auth;
        let mut rb = self
            .http
            .post(self.endpoint.responses_url())
            .bearer_auth(token)
            .header("Content-Type", "application/json")
            .header(
                "Accept",
                if request.stream == Some(true) {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .json(request);
        if let Endpoint::Codex {
            chatgpt_account_id,
            originator,
            ..
        } = &self.endpoint
        {
            rb = rb
                .header("chatgpt-account-id", chatgpt_account_id)
                .header("originator", originator);
        }
        rb.send().await.map_err(ClientError::Http)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("http: {0}")]
    Http(reqwest::Error),
    #[error("http status {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("json: {0}")]
    Json(serde_json::Error),
    #[error("sse: {0}")]
    Sse(crate::SseError),
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use futures::StreamExt;
    #[tokio::test]
    async fn live_public_api_minimal_text() {
        if std::env::var("MU_LIVE_OPENAI_API").ok().as_deref() != Some("1") {
            eprintln!("skipping live_public_api_minimal_text");
            return;
        }
        let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
        let c = Client::new(Endpoint::public_api(), Auth::Bearer(key));
        let mut req = CreateResponseRequest::text("gpt-4.1-mini", "Reply with exactly: pong");
        req.max_output_tokens = Some(16);
        let r = c.create_response(&req).await.expect("response");
        assert!(r.output_text().to_lowercase().contains("pong"), "{r:?}");
    }
    #[tokio::test]
    async fn live_public_api_stream_text() {
        if std::env::var("MU_LIVE_OPENAI_API").ok().as_deref() != Some("1") {
            eprintln!("skipping live_public_api_stream_text");
            return;
        }
        let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
        let c = Client::new(Endpoint::public_api(), Auth::Bearer(key));
        let mut req = CreateResponseRequest::text("gpt-4.1-mini", "Say hi.").streaming();
        req.max_output_tokens = Some(16);
        let s = c.stream_response(&req).await.expect("stream");
        futures::pin_mut!(s);
        let mut saw_text = false;
        let mut saw_done = false;
        while let Some(ev) = s.next().await {
            match ev.expect("event") {
                ResponseStreamEvent::OutputTextDelta { .. } => saw_text = true,
                ResponseStreamEvent::Completed { .. } => {
                    saw_done = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_text && saw_done);
    }
}
