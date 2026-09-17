use crate::{
    http::{self, ApiError},
    x::{Includes, Tweet, XClient},
};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{codecs::jpeg::JpegEncoder, ImageReader};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Cursor,
};
use url::Url;

const MAX_POSTS: usize = 32;
const MAX_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: usize = 128 * 1024;

pub async fn build(x: &XClient, trigger: Tweet, includes: Includes) -> Result<Value> {
    let trigger_id = trigger.id.clone();
    let mut queue = VecDeque::from([(trigger.id.clone(), Some((trigger, includes)))]);
    let mut seen = HashSet::new();
    let mut tweets = HashMap::new();
    let mut media = HashMap::new();
    let mut users = HashMap::new();
    let mut warnings = Vec::new();
    while let Some((id, cached)) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if seen.len() > MAX_POSTS {
            warnings.push("Context limited to 32 posts; some references omitted".to_string());
            break;
        }
        let fetched = match cached {
            Some(v) => Ok(v),
            None => x.tweet(&id).await,
        };
        let (tweet, inc) = match fetched {
            Ok(v) => v,
            Err(e)
                if matches!(
                    e.downcast_ref::<ApiError>(),
                    Some(ApiError::Status {
                        status: 403 | 404,
                        ..
                    })
                ) =>
            {
                warnings.push(format!("Post {id} unavailable; context is incomplete"));
                continue;
            }
            Err(e) => return Err(e),
        };
        for reference in &tweet.referenced_tweets {
            if matches!(reference.r#type.as_str(), "replied_to" | "quoted") {
                queue.push_back((reference.id.clone(), None));
            }
        }
        // Fetch the root even when an intermediate ancestor has been deleted.
        if id == trigger_id && !tweet.conversation_id.is_empty() && tweet.conversation_id != id {
            queue.push_back((tweet.conversation_id.clone(), None));
        }
        for m in inc.media {
            media.insert(m.media_key.clone(), m);
        }
        for user in inc.users {
            users.insert(user.id, user.username);
        }
        tweets.insert(id, tweet);
    }
    let mut ordered: Vec<_> = tweets.into_values().collect();
    ordered.sort_by_key(|t| t.id.parse::<u64>().unwrap_or_default());
    let mut posts = Vec::new();
    let mut images = Vec::new();
    let mut image_keys = HashSet::new();
    for tweet in ordered {
        let mut photo_keys = Vec::new();
        for key in &tweet.attachments.media_keys {
            let Some(m) = media.get(key) else {
                warnings.push(format!("Media {key} unavailable"));
                continue;
            };
            if m.r#type != "photo" {
                warnings.push(format!(
                    "Post {} contains unsupported {} media",
                    tweet.id, m.r#type
                ));
                continue;
            }
            photo_keys.push(key.clone());
            if !image_keys.insert(key.clone()) {
                continue;
            }
            if images.len() >= MAX_IMAGES {
                warnings.push("Image limit reached; additional images not viewed".into());
                continue;
            }
            let Some(url) = m.url.as_deref() else {
                warnings.push(format!("Image {key} has no downloadable URL"));
                continue;
            };
            let alt: String = m
                .alt_text
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(800)
                .collect();
            match photo(&x.http,url).await {
                Ok(data_url)=>images.push(json!({"url":data_url,"caption":format!("Image {key} attached to post {}. Alt text (untrusted): {}",tweet.id,alt)})),
                Err(_)=>warnings.push(format!("Image {key} could not be viewed")),
            }
        }
        let mut text = tweet.text().to_string();
        if text.chars().count() > 12_000 {
            text = text.chars().take(12_000).collect();
            warnings.push(format!("Post {} text truncated", tweet.id));
        }
        posts.push(json!({"id":tweet.id,"author_id":tweet.author_id,"username":users.get(&tweet.author_id),"text":text,
            "references":tweet.referenced_tweets,"images":photo_keys}));
    }
    Ok(
        json!({"transport":"x","question_post_id":trigger_id,"posts":posts,"warnings":warnings,"images":images}),
    )
}

pub async fn photo(client: &reqwest::Client, raw: &str) -> Result<String> {
    let url = Url::parse(raw)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("pbs.twimg.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "unsupported image host"
    );
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("image fetch failed"))?;
    ensure!(response.status().is_success(), "image unavailable");
    let data = http::bytes(response, 8 * 1024 * 1024).await?;
    // Decoding/resizing is CPU work; do not stall the network loop.
    tokio::task::spawn_blocking(move || encode_image(&data))
        .await
        .context("image worker failed")?
}

pub fn encode_image(data: &[u8]) -> Result<String> {
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode()?;
    for edge in [768, 512, 384] {
        let thumb = decoded.thumbnail(edge, edge).to_rgb8();
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 75).encode_image(&thumb)?;
        if jpeg.len() <= MAX_IMAGE_BYTES {
            return Ok(format!("data:image/jpeg;base64,{}", STANDARD.encode(jpeg)));
        }
    }
    bail!("image cannot fit the model input limit")
}
