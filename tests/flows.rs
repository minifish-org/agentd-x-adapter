use agentd_x_adapter::{
    agentd::Agentd,
    config::Config,
    context, http,
    oauth::Credentials,
    service::{reply_text, Service},
    store::Store,
    x::{Includes, Page, Tweet, User, XClient},
};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

fn mention(id: &str) -> Tweet {
    serde_json::from_value(json!({"id":id,"text":"@agentd_ai explain","author_id":"42","conversation_id":"100","entities":{"mentions":[{"id":"7","username":"agentd_ai"}]},"referenced_tweets":[{"type":"replied_to","id":"100"}]})).unwrap()
}
fn config(base: url::Url, path: std::path::PathBuf, publish: bool) -> Config {
    Config {
        agentd_url: base,
        agentd_token: "test".into(),
        tenant: "x-agentd".into(),
        agent: "x-bot".into(),
        owner_id: "42".into(),
        bot_username: "agentd_ai".into(),
        consumer_key: "key".into(),
        consumer_secret: "secret".into(),
        access_token: "token".into(),
        access_secret: "token-secret".into(),
        state: path,
        poll_secs: 60,
        publish,
    }
}
#[derive(Default)]
struct Mock {
    turns: Vec<Value>,
    posts: Vec<Value>,
    acks: Vec<Value>,
    post_status: u16,
    ack_fail_once: bool,
    claimed: bool,
    partial: bool,
}
type Shared = Arc<Mutex<Mock>>;
async fn mentions(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    assert!(q.contains_key("start_time"));
    if s.lock().unwrap().partial {
        return Json(json!({"errors":[{"title":"partial failure"}]}));
    }
    if !q.contains_key("pagination_token") {
        Json(json!({"data":[mention("102")],"meta":{"next_token":"page2"}}))
    } else {
        Json(json!({"data":[mention("101")]}))
    }
}
async fn root() -> Json<Value> {
    Json(
        json!({"data":{"id":"100","author_id":"55","text":"original post","conversation_id":"100","referenced_tweets":[{"type":"quoted","id":"99"}]}}),
    )
}
async fn quote() -> Json<Value> {
    Json(
        json!({"data":{"id":"99","author_id":"66","text":"quoted evidence","conversation_id":"99"}}),
    )
}
async fn trigger() -> Json<Value> {
    Json(json!({"data":mention("101")}))
}
async fn turns(State(s): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    s.lock().unwrap().turns.push(body);
    Json(json!({"run_id":"run-1","status":"queued"}))
}
async fn claim(State(s): State<Shared>) -> Json<Value> {
    let s = s.lock().unwrap();
    if s.claimed {
        Json(json!({"deliveries":[]}))
    } else {
        Json(
            json!({"deliveries":[{"delivery_id":"delivery-1","run_id":"run-1","claim_token":"claim","destination":"x:101","payload":{"reply":"看到了原推。"}}]}),
        )
    }
}
async fn ack(State(s): State<Shared>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut s = s.lock().unwrap();
    s.acks.push(body);
    let status = if s.ack_fail_once {
        s.ack_fail_once = false;
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        s.claimed = true;
        StatusCode::OK
    };
    (status, Json(json!({"ok":true})))
}
async fn create(State(s): State<Shared>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut s = s.lock().unwrap();
    s.posts.push(body);
    (
        StatusCode::from_u16(if s.post_status == 0 {
            201
        } else {
            s.post_status
        })
        .unwrap(),
        Json(json!({"data":{"id":"999"}})),
    )
}
async fn fixture(
    publish: bool,
) -> (
    Service,
    Shared,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let state = Arc::new(Mutex::new(Mock::default()));
    let app = Router::new()
        .route("/2/users/7/mentions", get(mentions))
        .route("/2/tweets/100", get(root))
        .route("/2/tweets/99", get(quote))
        .route("/2/tweets/101", get(trigger))
        .route("/2/tweets", post(create))
        .route("/v1/tenants/x-agentd/turns", post(turns))
        .route("/v1/tenants/x-agentd/deliveries/claim", post(claim))
        .route("/v1/tenants/x-agentd/deliveries/delivery-1/ack", post(ack))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = url::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let config = config(base.clone(), dir.path().join("state.db"), publish);
    let store = Store::open(&config.state).unwrap();
    store.bind("fixture").unwrap();
    let client = http::client().unwrap();
    let x = XClient::new(
        client.clone(),
        base,
        Credentials {
            key: "key".into(),
            secret: "secret".into(),
            token: "token".into(),
            token_secret: "token-secret".into(),
        },
    );
    let service = Service {
        agentd: Agentd::new(client, &config),
        x,
        store,
        config,
        bot: User {
            id: "7".into(),
            username: "agentd_ai".into(),
        },
    };
    (service, state, dir, server)
}

#[test]
fn admission_is_owner_and_explicit_mention_only() {
    let bot = User {
        id: "7".into(),
        username: "agentd_ai".into(),
    };
    let mut t = mention("101");
    assert!(t.eligible("42", &bot));
    t.note_tweet = Some(serde_json::from_value(json!({"text":"full text"})).unwrap());
    assert!(t.eligible("42", &bot));
    t.author_id = "43".into();
    assert!(!t.eligible("42", &bot));
    t.author_id = "42".into();
    t.entities.mentions.clear();
    assert!(!t.eligible("42", &bot));
    t.text = "this mentions @agentd_ai in plain text".into();
    assert!(!t.eligible("42", &bot));
}
#[test]
fn reply_rejects_overlong_text_and_unsolicited_mentions() {
    assert!(reply_text(&json!({"reply":"中".repeat(140)})).is_ok());
    for s in [
        "中".repeat(141),
        "@someone hi".into(),
        "https://evil.example".into(),
        " ".into(),
    ] {
        assert!(reply_text(&json!({"reply":s})).is_err());
    }
}
#[test]
fn image_normalization_produces_bounded_visual_input() {
    let image = image::RgbImage::from_pixel(1600, 1000, image::Rgb([250, 30, 10]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    let data = context::encode_image(bytes.get_ref()).unwrap();
    assert!(data.starts_with("data:image/jpeg;base64,"));
    assert!(data.len() < 180000);
    assert!(context::encode_image(b"not an image").is_err());
}
#[tokio::test]
async fn all_pages_persist_before_cursor_and_context_includes_root_and_quote() {
    let (s, m, _dir, server) = fixture(false).await;
    s.poll().await.unwrap();
    assert_eq!(s.store.get("since").unwrap().as_deref(), Some("102"));
    assert_eq!(s.store.ready().unwrap(), vec!["101", "102"]);
    s.store.defer("102", 600).unwrap();
    s.jobs().await.unwrap();
    let m = m.lock().unwrap();
    assert_eq!(m.turns.len(), 1);
    let body = &m.turns[0];
    assert!(body.get("delivery").is_none());
    assert_eq!(body["request_id"], "x:101");
    assert_eq!(body["payload"]["posts"][0]["text"], "quoted evidence");
    assert_eq!(body["payload"]["posts"][1]["text"], "original post");
    assert!(m.posts.is_empty());
    server.abort();
}
#[tokio::test]
async fn partial_response_does_not_advance_cursor() {
    let (s, m, _dir, server) = fixture(false).await;
    m.lock().unwrap().partial = true;
    assert!(s.poll().await.is_err());
    assert!(s.store.get("since").unwrap().is_none());
    server.abort();
}
#[tokio::test]
async fn ack_failure_never_reposts_a_successful_reply() {
    let (s, m, _dir, server) = fixture(true).await;
    s.store
        .enqueue(&mention("101"), &Includes::default(), true)
        .unwrap();
    s.store.submitted("101", "run-1", true).unwrap();
    m.lock().unwrap().ack_fail_once = true;
    assert!(s.deliver().await.is_err());
    assert_eq!(s.store.job("101").unwrap().unwrap().state, "sent");
    s.deliver().await.unwrap();
    let m = m.lock().unwrap();
    assert_eq!(m.posts.len(), 1);
    assert_eq!(m.posts[0]["reply"]["in_reply_to_tweet_id"], "101");
    server.abort();
}
#[tokio::test]
async fn ambiguous_post_is_quarantined_and_not_retried() {
    let (s, m, _dir, server) = fixture(true).await;
    s.store
        .enqueue(&mention("101"), &Includes::default(), true)
        .unwrap();
    s.store.submitted("101", "run-1", true).unwrap();
    m.lock().unwrap().post_status = 503;
    s.deliver().await.unwrap();
    assert_eq!(s.store.job("101").unwrap().unwrap().state, "unknown");
    m.lock().unwrap().claimed = false;
    s.deliver().await.unwrap();
    assert_eq!(m.lock().unwrap().posts.len(), 1);
    server.abort();
}
#[tokio::test]
async fn preview_jobs_cannot_publish_after_mode_switch() {
    let (s, m, _dir, server) = fixture(true).await;
    s.store
        .enqueue(&mention("101"), &Includes::default(), false)
        .unwrap();
    s.store.submitted("101", "run-1", false).unwrap();
    s.deliver().await.unwrap();
    assert!(m.lock().unwrap().posts.is_empty());
    server.abort();
}
#[test]
fn crash_recovery_quarantines_inflight_posts_and_locks_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    {
        let s = Store::open(&path).unwrap();
        s.bind("first").unwrap();
        assert!(s.bind("other").is_err());
        assert!(Store::open(&path).is_err());
        s.enqueue(&mention("101"), &Includes::default(), true)
            .unwrap();
        s.state("101", "sending").unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.job("101").unwrap().unwrap().state, "unknown");
}
#[test]
fn note_tweet_content_is_preserved() {
    let p: Page = serde_json::from_value(
        json!({"data":[{"id":"101","text":"truncated","note_tweet":{"text":"full long post"}}]}),
    )
    .unwrap();
    assert_eq!(p.data[0].text(), "full long post");
}
