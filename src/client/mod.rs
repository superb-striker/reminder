//! A thin wrapper around `reqwest` for talking to the reminder server.
//!
//! Every CLI subcommand except `serve` (and the debug-only `tick` /
//! `tick-loop`) goes through this client rather than touching the
//! local database directly -- the server is the source of truth (see
//! the project brief), so the laptop CLI is just another client of it,
//! the same way the phone is.

pub mod tui;
pub mod watch;

use crate::model::{NewReminder, Reminder, ReminderPatch};
use reqwest::StatusCode;

pub struct ApiClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl ApiClient {
    pub fn new(base_url: String, token: String) -> Self {
        // Trim a trailing slash so `format!("{base_url}/reminders")`
        // never produces a double slash.
        let base_url = base_url.trim_end_matches('/').to_string();
        ApiClient {
            http: reqwest::Client::new(),
            base_url,
            token,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(&self.token)
    }

    /// Turn a non-success response into a readable error, including
    /// the server's response body when there is one -- this is what
    /// surfaces "reminder not found" / auth failures to the CLI user
    /// instead of a bare status code.
    async fn check_status(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "server returned {status}{}",
            if body.is_empty() {
                String::new()
            } else {
                format!(": {body}")
            }
        )
    }

    pub async fn create_reminder(&self, new: &NewReminder) -> anyhow::Result<Reminder> {
        let resp = self
            .auth(self.http.post(self.url("/reminders")))
            .json(new)
            .send()
            .await?;
        Ok(Self::check_status(resp).await?.json().await?)
    }

    pub async fn list_reminders(&self) -> anyhow::Result<Vec<Reminder>> {
        let resp = self.auth(self.http.get(self.url("/reminders"))).send().await?;
        Ok(Self::check_status(resp).await?.json().await?)
    }

    pub async fn get_reminder(&self, id: i64) -> anyhow::Result<Option<Reminder>> {
        let resp = self
            .auth(self.http.get(self.url(&format!("/reminders/{id}"))))
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::check_status(resp).await?.json().await?))
    }

    pub async fn update_reminder(
        &self,
        id: i64,
        patch: &ReminderPatch,
    ) -> anyhow::Result<Option<Reminder>> {
        let resp = self
            .auth(self.http.put(self.url(&format!("/reminders/{id}"))))
            .json(patch)
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::check_status(resp).await?.json().await?))
    }

    /// Returns whether a reminder was actually found and deleted.
    pub async fn delete_reminder(&self, id: i64) -> anyhow::Result<bool> {
        let resp = self
            .auth(self.http.delete(self.url(&format!("/reminders/{id}"))))
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        Self::check_status(resp).await?;
        Ok(true)
    }

    /// Returns whether there was a fired occurrence to mark complete.
    pub async fn complete_reminder(&self, id: i64) -> anyhow::Result<bool> {
        let resp = self
            .auth(self.http.post(self.url(&format!("/reminders/{id}/complete"))))
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        Self::check_status(resp).await?;
        Ok(true)
    }

    pub async fn pending_laptop_occurrences(&self) -> anyhow::Result<Vec<crate::model::PendingLaptopOccurrence>> {
        let resp = self
            .auth(self.http.get(self.url("/occurrences/pending-laptop")))
            .send()
            .await?;
        Ok(Self::check_status(resp).await?.json().await?)
    }

    pub async fn ack_laptop_occurrence(&self, occurrence_id: i64) -> anyhow::Result<()> {
        let resp = self
            .auth(
                self.http
                    .post(self.url(&format!("/occurrences/{occurrence_id}/ack-laptop"))),
            )
            .send()
            .await?;
        Self::check_status(resp).await?;
        Ok(())
    }
}
