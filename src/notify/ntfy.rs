//! Push notifications via [ntfy](https://ntfy.sh): `POST` the message
//! body to `{server}/{topic}`, with the title in a header. Works
//! unauthenticated against the public `ntfy.sh` server (pick any
//! unguessable topic name -- treat it like a password, since anyone
//! who knows it can read your notifications) or against a self-hosted
//! instance with a bearer token.

use crate::notify::NotificationBackend;

pub struct NtfyBackend {
    http: reqwest::Client,
    server: String,
    topic: String,
    token: Option<String>,
}

impl NtfyBackend {
    /// `server` is the base URL, e.g. `"https://ntfy.sh"` (a trailing
    /// slash is tolerated). `token` is only needed for a self-hosted
    /// instance with access control enabled -- the public server
    /// doesn't use one.
    pub fn new(server: String, topic: String, token: Option<String>) -> Self {
        NtfyBackend {
            http: reqwest::Client::new(),
            server: server.trim_end_matches('/').to_string(),
            topic,
            token,
        }
    }
}

impl NotificationBackend for NtfyBackend {
    async fn send(&self, title: &str, body: &str) -> anyhow::Result<()> {
        let url = format!("{}/{}", self.server, self.topic);

        // ntfy requires the Title header to be ASCII (non-ASCII needs
        // RFC 2047 encoding, which isn't worth adding support for
        // here) -- a reminder title with, say, an emoji in it would
        // otherwise make the whole request fail to build. Falling
        // back to putting the title in the body instead degrades
        // gracefully rather than losing the notification entirely.
        let mut req = self.http.post(&url);
        req = if title.is_ascii() {
            req.header("Title", title).body(body.to_string())
        } else {
            req.body(format!("{title}\n{body}"))
        };
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            anyhow::bail!("ntfy server returned {status}: {body_text}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, http::HeaderMap, routing::post, Router};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Option<(HeaderMap, String)>>>);

    /// Start a throwaway local HTTP server that just records whatever
    /// request it receives (and always answers 200), so `NtfyBackend`
    /// can be tested against something real instead of a mock crate.
    async fn start_capturing_server() -> (String, Captured) {
        let captured = Captured::default();
        let state = captured.clone();

        let app = Router::new().route(
            "/mytopic",
            post(
                |State(state): State<Captured>, headers: HeaderMap, body: String| async move {
                    *state.0.lock().unwrap() = Some((headers, body));
                    axum::http::StatusCode::OK
                },
            ),
        )
        .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("http://{addr}"), captured)
    }

    #[tokio::test]
    async fn sends_title_header_and_body_to_the_configured_topic() {
        let (server, captured) = start_capturing_server().await;
        let backend = NtfyBackend::new(server, "mytopic".into(), None);

        backend.send("Study Go", "It's time!").await.unwrap();

        let (headers, body) = captured.0.lock().unwrap().take().unwrap();
        assert_eq!(headers.get("Title").unwrap(), "Study Go");
        assert_eq!(body, "It's time!");
        assert!(headers.get("Authorization").is_none());
    }

    #[tokio::test]
    async fn includes_bearer_token_when_configured() {
        let (server, captured) = start_capturing_server().await;
        let backend = NtfyBackend::new(server, "mytopic".into(), Some("secret-token".into()));

        backend.send("Study Go", "It's time!").await.unwrap();

        let (headers, _) = captured.0.lock().unwrap().take().unwrap();
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer secret-token");
    }

    #[tokio::test]
    async fn non_ascii_title_falls_back_to_body_instead_of_failing() {
        let (server, captured) = start_capturing_server().await;
        let backend = NtfyBackend::new(server, "mytopic".into(), None);

        backend.send("Étude 📚", "It's time!").await.unwrap();

        let (headers, body) = captured.0.lock().unwrap().take().unwrap();
        assert!(headers.get("Title").is_none());
        assert_eq!(body, "Étude 📚\nIt's time!");
    }

    #[tokio::test]
    async fn non_success_status_is_reported_as_an_error() {
        let app = Router::new().route(
            "/mytopic",
            post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let backend = NtfyBackend::new(format!("http://{addr}"), "mytopic".into(), None);
        let err = backend.send("Study Go", "It's time!").await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }
}
