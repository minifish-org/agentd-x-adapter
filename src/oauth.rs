use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use url::Url;

// RFC 5849 percent encoding, not form encoding ('+' is never a space here).
pub fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Clone)]
pub struct Credentials {
    pub key: String,
    pub secret: String,
    pub token: String,
    pub token_secret: String,
}

impl Credentials {
    pub fn header(&self, method: &str, url: &Url, nonce: &str, timestamp: &str) -> String {
        let mut oauth = vec![
            ("oauth_consumer_key".to_string(), self.key.clone()),
            ("oauth_nonce".into(), nonce.into()),
            ("oauth_signature_method".into(), "HMAC-SHA1".into()),
            ("oauth_timestamp".into(), timestamp.into()),
            ("oauth_token".into(), self.token.clone()),
            ("oauth_version".into(), "1.0".into()),
        ];
        // JSON request bodies are deliberately excluded from OAuth1 parameters.
        let mut params: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| (encode(&k), encode(&v)))
            .chain(oauth.iter().map(|(k, v)| (encode(k), encode(v))))
            .collect();
        params.sort();
        let normalized = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let mut base_url = url.clone();
        base_url.set_query(None);
        base_url.set_fragment(None);
        let base = format!(
            "{}&{}&{}",
            method.to_uppercase(),
            encode(base_url.as_str()),
            encode(&normalized)
        );
        let key = format!("{}&{}", encode(&self.secret), encode(&self.token_secret));
        let mut mac =
            Hmac::<Sha1>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
        mac.update(base.as_bytes());
        oauth.push((
            "oauth_signature".into(),
            STANDARD.encode(mac.finalize().into_bytes()),
        ));
        format!(
            "OAuth {}",
            oauth
                .iter()
                .map(|(k, v)| format!("{}=\"{}\"", encode(k), encode(v)))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn published_oauth_photo_example() {
        // Public OAuth 1.0 Appendix A.5 test vector, not live credentials.
        // https://oauth.net/core/1.0/#anchor30
        let c = Credentials {
            key: "dpf43f3p2l4k3l03".into(),
            secret: "kd94hf93k423kf44".into(),
            token: "nnch734d00sl2jdk".into(),
            token_secret: "pfkkdhi9sl3r4s00".into(),
        };
        let h = c.header(
            "GET",
            &Url::parse("http://photos.example.net/photos?file=vacation.jpg&size=original")
                .unwrap(),
            "kllo9940pd9333jh",
            "1191242096",
        );
        assert!(h.contains("tR3%2BTy81lMeYAr%2FFid0kMTYa%2FWM%3D"));
        assert_eq!(encode("中 +/~"), "%E4%B8%AD%20%2B%2F~");
    }
}
