use anyhow::{bail, Context, Result};
use std::{env, path::PathBuf};
use url::Url;

#[derive(Clone)]
pub struct Config {
    pub agentd_url: Url,
    pub agentd_token: String,
    pub tenant: String,
    pub agent: String,
    pub owner_id: String,
    pub bot_username: String,
    pub consumer_key: String,
    pub consumer_secret: String,
    pub access_token: String,
    pub access_secret: String,
    pub state: PathBuf,
    pub poll_secs: u64,
    pub publish: bool,
}

fn required(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .with_context(|| format!("set {name}"))
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_digit())
        && id.parse::<u64>().is_ok_and(|n| n > 0)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let agentd_url = Url::parse(&required("AGENTD_URL")?).context("invalid AGENTD_URL")?;
        if !matches!(agentd_url.scheme(), "https" | "http")
            || agentd_url.host_str().is_none()
            || !agentd_url.username().is_empty()
            || agentd_url.password().is_some()
            || agentd_url.query().is_some()
            || agentd_url.fragment().is_some()
        {
            bail!("AGENTD_URL must be an HTTP(S) base URL without credentials, query or fragment");
        }
        let owner_id = env::var("X_OWNER_ID").unwrap_or_default();
        if !owner_id.is_empty() && !valid_id(&owner_id) {
            bail!("X_OWNER_ID must be a numeric user ID");
        }
        let poll_secs = env::var("POLL_SECS")
            .unwrap_or_else(|_| "60".into())
            .parse::<u64>()?;
        if !(30..=3600).contains(&poll_secs) {
            bail!("POLL_SECS must be 30..3600");
        }
        let publish = match env::var("X_PUBLISH").as_deref().unwrap_or("false") {
            "true" => true,
            "false" => false,
            _ => bail!("X_PUBLISH must be true or false"),
        };
        if publish && env::var("X_AI_REPLY_APPROVED").as_deref() != Ok("true") {
            bail!("publishing requires X_AI_REPLY_APPROVED=true after X approval");
        }
        let tenant = env::var("TENANT").unwrap_or_else(|_| "x-agentd".into());
        let agent = env::var("AGENT_REF").unwrap_or_else(|_| "x-bot".into());
        for name in [&tenant, &agent] {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                bail!("TENANT and AGENT_REF must be simple names");
            }
        }
        Ok(Self {
            agentd_url,
            agentd_token: required("AGENTD_TOKEN")?,
            tenant,
            agent,
            owner_id,
            bot_username: env::var("X_BOT_USERNAME").unwrap_or_else(|_| "agentd_ai".into()),
            consumer_key: required("X_API_KEY")?,
            consumer_secret: required("X_API_KEY_SECRET")?,
            access_token: required("X_ACCESS_TOKEN")?,
            access_secret: required("X_ACCESS_TOKEN_SECRET")?,
            state: env::var("STATE_PATH")
                .unwrap_or_else(|_| "state/adapter.db".into())
                .into(),
            poll_secs,
            publish,
        })
    }
}
