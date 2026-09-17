use crate::{
    agentd::Agentd,
    config::{valid_id, Config},
    context,
    http::ApiError,
    store::Store,
    x::{User, XClient},
};
use anyhow::{bail, ensure, Result};
use serde_json::Value;

pub struct Service {
    pub config: Config,
    pub x: XClient,
    pub agentd: Agentd,
    pub store: Store,
    pub bot: User,
}
impl Service {
    pub async fn poll(&self) -> Result<()> {
        let since = self.store.get("since")?;
        let start = self
            .store
            .get("start")?
            .ok_or_else(|| anyhow::anyhow!("missing start cursor"))?;
        let mut next = None;
        let mut highest = since
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let mut page_tokens = std::collections::HashSet::new();
        for _ in 0..20 {
            let page = self
                .x
                .mentions(&self.bot.id, since.as_deref(), &start, next.as_deref())
                .await?;
            ensure!(
                page.errors.is_empty(),
                "partial mentions response; cursor unchanged"
            );
            for tweet in page.data {
                ensure!(valid_id(&tweet.id), "invalid post ID in mentions");
                highest = highest.max(tweet.id.parse()?);
                if tweet.eligible(&self.config.owner_id, &self.bot) {
                    self.store
                        .enqueue(&tweet, &page.includes, self.config.publish)?;
                }
            }
            next = page.meta.next_token;
            if next.is_none() {
                if highest > 0 {
                    self.store.set("since", &highest.to_string())?;
                }
                return Ok(());
            }
            ensure!(
                page_tokens.insert(next.clone()),
                "repeated mentions pagination token"
            );
        }
        bail!("mentions exceed 20 pages; cursor unchanged; narrow initial start time")
    }

    pub async fn jobs(&self) -> Result<()> {
        for id in self.store.ready()? {
            if let Err(error) = self.job(&id).await {
                tracing::warn!(post_id=%id,error=%error,"job deferred");
                let delay = retry_delay(&error);
                self.store.defer(&id, delay)?;
            }
        }
        self.store.prune()?;
        Ok(())
    }
    async fn job(&self, id: &str) -> Result<()> {
        let job = self
            .store
            .job(id)?
            .ok_or_else(|| anyhow::anyhow!("missing job"))?;
        if job.state == "preview_wait" {
            let run = self
                .agentd
                .preview(
                    job.run_id
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("missing run"))?,
                )
                .await?;
            match run["status"].as_str() {
                Some("succeeded") => {
                    self.store.preview_done(id, &run["output"])?;
                    tracing::info!(post_id = id, "preview ready; inspect with status");
                }
                Some("failed" | "cancelled") => {
                    self.store.state(id, "failed")?;
                    tracing::warn!(post_id = id, "agentd run failed");
                }
                _ => self.store.defer(id, 10)?,
            }
            return Ok(());
        }
        ensure!(
            job.tweet.eligible(&self.config.owner_id, &self.bot),
            "job is not an allowed mention"
        );
        // Persist the exact body before submission. A timed-out POST can safely be resubmitted
        // with the same tenant-scoped idempotency key, even if the source post later changes.
        let payload = match self.store.payload(id)? {
            Some(p) => p,
            None => {
                let p = context::build(&self.x, job.tweet.clone(), job.includes).await?;
                self.store.save_payload(id, &p)?;
                p
            }
        };
        let conversation = if job.tweet.conversation_id.is_empty() {
            id
        } else {
            &job.tweet.conversation_id
        };
        let run = self
            .agentd
            .submit(id, conversation, payload, job.publish)
            .await?;
        self.store.submitted(id, &run, job.publish)?;
        tracing::info!(post_id=id,run_id=%run,publish=job.publish,"turn accepted");
        Ok(())
    }

    pub async fn deliver(&self) -> Result<()> {
        // Pausing publishing also pauses outbox claiming; no queued preview is ever published.
        if !self.config.publish {
            return Ok(());
        }
        for delivery in self.agentd.claim().await? {
            let Some(id) = delivery
                .destination
                .strip_prefix("x:")
                .filter(|id| valid_id(id))
            else {
                self.agentd
                    .ack(
                        &delivery,
                        "failed",
                        Some("foreign destination; dedicated X tenant required"),
                        0,
                    )
                    .await?;
                continue;
            };
            let Some(job) = self.store.job(id)? else {
                self.agentd
                    .ack(&delivery, "failed", Some("no admitted owner mention"), 0)
                    .await?;
                continue;
            };
            if !job.publish || job.run_id.as_deref() != Some(&delivery.run_id) {
                self.agentd
                    .ack(
                        &delivery,
                        "failed",
                        Some("run does not match admitted mention"),
                        0,
                    )
                    .await?;
                continue;
            }
            if job.state == "sent" {
                self.agentd.ack(&delivery, "delivered", None, 0).await?;
                continue;
            }
            if matches!(job.state.as_str(), "sending" | "unknown" | "failed") {
                self.agentd
                    .ack(
                        &delivery,
                        "failed",
                        Some("delivery requires operator review; no automatic repost"),
                        0,
                    )
                    .await?;
                continue;
            }
            // A freshly fetched trigger must still belong to the owner and explicitly mention us.
            match self.x.tweet(id).await {
                Ok((tweet, _)) if tweet.eligible(&self.config.owner_id, &self.bot) => {}
                Ok(_) => {
                    self.store.state(id, "failed")?;
                    self.agentd
                        .ack(&delivery, "failed", Some("mention no longer eligible"), 0)
                        .await?;
                    continue;
                }
                Err(e) => {
                    if matches!(
                        e.downcast_ref::<ApiError>(),
                        Some(ApiError::Status {
                            status: 403 | 404,
                            ..
                        })
                    ) {
                        self.store.state(id, "failed")?;
                        self.agentd
                            .ack(&delivery, "failed", Some("trigger unavailable"), 0)
                            .await?;
                    } else {
                        self.agentd
                            .ack(
                                &delivery,
                                "retry",
                                Some("trigger check failed"),
                                retry_delay(&e),
                            )
                            .await?;
                    }
                    continue;
                }
            }
            let text = match reply_text(&delivery.payload) {
                Ok(s) => s,
                Err(_) => {
                    self.store.state(id, "failed")?;
                    self.agentd
                        .ack(
                            &delivery,
                            "failed",
                            Some("reply empty, oversized, or contains mention/link"),
                            0,
                        )
                        .await?;
                    continue;
                }
            };
            self.store.state(id, "sending")?;
            match self.x.reply(id, &text).await {
                Ok(reply) => {
                    self.store.sent(id, &reply)?;
                    self.agentd.ack(&delivery, "delivered", None, 0).await?;
                }
                Err(ApiError::Status {
                    status: 429,
                    retry_secs,
                }) => {
                    self.store.state(id, "submitted")?;
                    self.agentd
                        .ack(&delivery, "retry", Some("X rate limit"), retry_secs)
                        .await?;
                }
                Err(ApiError::Status {
                    status: 400 | 401 | 402 | 403 | 404 | 422,
                    ..
                }) => {
                    self.store.state(id, "failed")?;
                    self.agentd
                        .ack(&delivery, "failed", Some("X rejected reply"), 0)
                        .await?;
                }
                Err(_) => {
                    self.store.state(id, "unknown")?;
                    tracing::error!(
                        post_id = id,
                        "reply outcome unknown; review X before any manual recovery"
                    );
                    self.agentd
                        .ack(
                            &delivery,
                            "failed",
                            Some("X send outcome unknown; review manually"),
                            0,
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }
}

pub fn retry_delay(error: &anyhow::Error) -> u64 {
    match error.downcast_ref::<ApiError>() {
        Some(ApiError::Status { retry_secs, .. }) => *retry_secs,
        _ => 60,
    }
}

pub fn reply_text(payload: &Value) -> Result<String> {
    let text = payload
        .get("reply")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("reply must be a string"))?
        .trim();
    // Conservative upper bound: each Unicode scalar counts as two, guaranteeing <=280.
    // Do not silently truncate or split an answer into unsolicited follow-up posts.
    ensure!(
        !text.is_empty() && text.chars().count() <= 140,
        "reply exceeds conservative X limit"
    );
    ensure!(
        !text.contains('@')
            && !text.contains("http://")
            && !text.contains("https://")
            && !text.contains("www."),
        "reply contains unsolicited mention or link"
    );
    Ok(text.to_string())
}
