use crate::{
    config::valid_id,
    http::{self, ApiError},
    oauth::Credentials,
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Tweet {
    pub id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub author_id: String,
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub referenced_tweets: Vec<Reference>,
    #[serde(default)]
    pub attachments: Attachments,
    #[serde(default)]
    pub entities: Entities,
    #[serde(default, alias = "note_post")]
    pub note_tweet: Option<Note>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Reference {
    pub id: String,
    pub r#type: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Attachments {
    #[serde(default)]
    pub media_keys: Vec<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Entities {
    #[serde(default)]
    pub mentions: Vec<Mention>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Mention {
    pub username: String,
    #[serde(default)]
    pub id: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Note {
    pub text: String,
    #[serde(default)]
    pub entities: Entities,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Media {
    pub media_key: String,
    pub r#type: String,
    pub url: Option<String>,
    pub alt_text: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct User {
    pub id: String,
    pub username: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Includes {
    #[serde(default)]
    pub media: Vec<Media>,
    #[serde(default)]
    pub users: Vec<User>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Page {
    #[serde(default)]
    pub data: Vec<Tweet>,
    #[serde(default)]
    pub includes: Includes,
    #[serde(default)]
    pub meta: Meta,
    #[serde(default)]
    pub errors: Vec<Value>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Meta {
    pub next_token: Option<String>,
}

impl Tweet {
    pub fn text(&self) -> &str {
        self.note_tweet
            .as_ref()
            .map(|n| n.text.as_str())
            .unwrap_or(&self.text)
    }
    pub fn eligible(&self, owner: &str, bot: &User) -> bool {
        let mentions = self.entities.mentions.iter().chain(
            self.note_tweet
                .iter()
                .flat_map(|n| n.entities.mentions.iter()),
        );
        self.author_id == owner
            && self.author_id != bot.id
            && valid_id(&self.id)
            && !self
                .referenced_tweets
                .iter()
                .any(|r| r.r#type == "retweeted")
            && mentions.into_iter().any(|m| {
                if m.id.is_empty() {
                    m.username.eq_ignore_ascii_case(&bot.username)
                } else {
                    m.id == bot.id
                }
            })
    }
}

pub struct XClient {
    pub http: reqwest::Client,
    pub base: Url,
    credentials: Credentials,
}
impl XClient {
    pub fn new(http: reqwest::Client, base: Url, credentials: Credentials) -> Self {
        Self {
            http,
            base,
            credentials,
        }
    }
    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<T, ApiError> {
        let mut url = self.base.join(path).map_err(|_| ApiError::InvalidJson)?;
        if !params.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let authorization = self.credentials.header(
            method.as_str(),
            &url,
            &uuid::Uuid::new_v4().simple().to_string(),
            &chrono::Utc::now().timestamp().to_string(),
        );
        let mut request = self
            .http
            .request(method, url)
            .header("Authorization", authorization);
        if let Some(body) = body {
            request = request.json(&body);
        }
        http::json(request.send().await.map_err(|_| ApiError::Transport)?).await
    }
    pub async fn me(&self) -> Result<User, ApiError> {
        #[derive(Deserialize)]
        struct Response {
            data: User,
        }
        Ok(self
            .request::<Response>(reqwest::Method::GET, "2/users/me", &[], None)
            .await?
            .data)
    }
    pub async fn user(&self, username: &str) -> Result<User> {
        ensure!(
            !username.is_empty()
                && username
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid username"
        );
        #[derive(Deserialize)]
        struct Response {
            data: User,
        }
        Ok(self
            .request::<Response>(
                reqwest::Method::GET,
                &format!("2/users/by/username/{username}"),
                &[],
                None,
            )
            .await?
            .data)
    }
    pub async fn mentions(
        &self,
        id: &str,
        since: Option<&str>,
        start: &str,
        page: Option<&str>,
    ) -> Result<Page> {
        ensure!(valid_id(id), "invalid user ID");
        let mut params = fields();
        params.push(("max_results", "100".into()));
        if let Some(since) = since {
            params.push(("since_id", since.into()));
        } else {
            params.push(("start_time", start.into()));
        }
        if let Some(page) = page {
            params.push(("pagination_token", page.into()));
        }
        Ok(self
            .request(
                reqwest::Method::GET,
                &format!("2/users/{id}/mentions"),
                &params,
                None,
            )
            .await?)
    }
    pub async fn tweet(&self, id: &str) -> Result<(Tweet, Includes)> {
        ensure!(valid_id(id), "invalid tweet ID");
        #[derive(Deserialize)]
        struct Response {
            data: Tweet,
            #[serde(default)]
            includes: Includes,
        }
        let response: Response = self
            .request(
                reqwest::Method::GET,
                &format!("2/tweets/{id}"),
                &fields(),
                None,
            )
            .await?;
        Ok((response.data, response.includes))
    }
    pub async fn reply(&self, id: &str, text: &str) -> Result<String, ApiError> {
        #[derive(Deserialize)]
        struct Response {
            data: Created,
        }
        #[derive(Deserialize)]
        struct Created {
            id: String,
        }
        let response: Response = self
            .request(
                reqwest::Method::POST,
                "2/tweets",
                &[],
                Some(json!({"text":text,"reply":{"in_reply_to_tweet_id":id}})),
            )
            .await?;
        if !valid_id(&response.data.id) {
            return Err(ApiError::InvalidJson);
        }
        Ok(response.data.id)
    }
}
fn fields() -> Vec<(&'static str, String)> {
    vec![
        (
            "tweet.fields",
            "author_id,conversation_id,referenced_tweets,attachments,entities,note_tweet".into(),
        ),
        ("expansions", "author_id,attachments.media_keys".into()),
        ("media.fields", "type,url,alt_text".into()),
        ("user.fields", "username".into()),
    ]
}
