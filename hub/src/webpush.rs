//! Web Push for browsers (Hubchat Web, feature `webpush`): what one
//! notification says, and the signature a push service asks for.
//!
//! A browser's push must show something (a silent one can cost the
//! subscription), so unlike the content-free UnifiedPush `wake` this carries
//! the notification itself, encrypted to the browser (RFC 8291) by
//! `push.rs`. Nothing here touches the database or the network: `push.rs`
//! reads the unread mail and sends; this module only words it and signs.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use serde_json::{json, Value};

/// How long a push service may trust one signature (RFC 8292 allows 24 h).
pub const VAPID_VALID_SECS: i64 = 12 * 3600;
pub const TITLE_MAX: usize = 80;
pub const BODY_MAX: usize = 160;
/// The whole payload, as compact JSON.
pub const PAYLOAD_MAX: usize = 2000;
pub const BADGE_MAX: i64 = 999;
pub const NAVIGATE_MAX: usize = 300;
/// The `sub` of a signature when the operator names no contact.
pub const DEFAULT_CONTACT: &str = "https://github.com/Maurdekye/orgtree-mailhub";

/// One unread message, as far as a notification says anything of it.
pub struct Note {
    pub from: String,
    /// The sender's name; empty when it has none (the address stands in).
    pub name: String,
    /// The reader's translation when one arrived, otherwise the body as
    /// written.
    pub text: String,
    /// The names of its files.
    pub files: Vec<String>,
}

impl Note {
    fn who(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.from
        } else {
            self.name.trim()
        }
    }

    fn says(&self) -> String {
        let text = self.text.trim();
        if !text.is_empty() {
            return text.to_string();
        }
        match self.files.as_slice() {
            [] => "New message".to_string(),
            [one] => format!("Sent a file: {one}"),
            many => format!("Sent {} files", many.len()),
        }
    }
}

/// At most `max` characters, the last one `…` when something was cut.
/// Line breaks become spaces: a notification is one line of each.
pub fn cut(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// The payload of one notification: `notes` newest first and not empty,
/// `navigate` the web app's own address as registered, `unread` the
/// address's unread count. The member names are WebKit's (Declarative Web
/// Push); a service worker reads the same JSON.
pub fn payload(notes: &[Note], navigate: &str, preview: bool, unread: i64) -> String {
    let newest = &notes[0];
    let one_sender = notes.iter().all(|n| n.from == newest.from);
    let count = notes.len();
    let (title, body) = if !preview {
        ("Hubchat".to_string(), if count == 1 { "New message".to_string() } else { format!("{count} new messages") })
    } else if one_sender {
        let more = if count > 1 { format!(" (+{} more)", count - 1) } else { String::new() };
        // the count is never cut off: the text gives way to it
        let text = cut(&newest.says(), BODY_MAX.saturating_sub(more.chars().count()));
        (newest.who().to_string(), text + &more)
    } else {
        let mut senders: Vec<&Note> = Vec::new();
        for n in notes {
            if !senders.iter().any(|s| s.from == n.from) {
                senders.push(n);
            }
        }
        let names: Vec<&str> = senders.iter().take(2).map(|n| n.who()).collect();
        let body = match senders.len() - names.len() {
            0 => format!("From {}", names.join(" and ")),
            1 => format!("From {} and 1 other", names.join(", ")),
            k => format!("From {} and {k} others", names.join(", ")),
        };
        (format!("{count} new messages"), body)
    };
    // one sender whose name may be shown: the notification opens that chat.
    // With the text switched off not even the sender's address goes along.
    let (to, tag) = if !preview {
        (navigate.to_string(), "hc-new".to_string())
    } else if one_sender {
        (format!("{navigate}#/chat/{}", newest.from), format!("hc-{}", newest.from))
    } else {
        (navigate.to_string(), "hc-many".to_string())
    };
    let build = |body: &str| {
        json!({
            "web_push": 8030,
            "notification": { "title": cut(&title, TITLE_MAX), "body": body, "navigate": to, "tag": tag, "silent": false },
            "app_badge": unread.clamp(0, BADGE_MAX),
        })
        .to_string()
    };
    let mut body = cut(&body, BODY_MAX);
    let mut out = build(&body);
    // long names in a script of four-byte characters: the text gives way
    while out.len() > PAYLOAD_MAX && body.chars().count() > 1 {
        body = cut(&body, body.chars().count() / 2);
        out = build(&body);
    }
    out
}

/// The web app's own address a notification opens: `https://`, or with
/// `local` (tests) `http://localhost` or `http://127.0.0.1`; no credentials,
/// query or fragment; at most `NAVIGATE_MAX` bytes. Its text as stored: it
/// ends with `/`.
pub fn navigate(value: &str, local: bool) -> Result<String, &'static str> {
    const WRONG: &str = "navigate must be the web app's https:// address, at most 300 bytes, without credentials, query or fragment";
    if value.len() > NAVIGATE_MAX || value.bytes().any(|b| b.is_ascii_control() || b.is_ascii_whitespace()) {
        return Err(WRONG);
    }
    let url = reqwest::Url::parse(value).map_err(|_| WRONG)?;
    let test_http = local && url.scheme() == "http" && url.host_str().is_some_and(|h| matches!(h, "localhost" | "127.0.0.1"));
    if (url.scheme() != "https" && !test_http)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(WRONG);
    }
    let mut text = url.to_string();
    if !text.ends_with('/') {
        text.push('/');
    }
    if text.len() > NAVIGATE_MAX {
        return Err(WRONG);
    }
    Ok(text)
}

/// The signing key a browser hands over: 43 base64url characters, the 32
/// bytes of a P-256 private key.
pub fn vapid_key(value: &str) -> Result<Vec<u8>, &'static str> {
    const WRONG: &str = "vapid must be a base64url P-256 private key (43 characters)";
    let bytes = Some(value).filter(|s| s.len() == 43).and_then(|s| URL_SAFE_NO_PAD.decode(s).ok()).filter(|b| b.len() == 32).ok_or(WRONG)?;
    SigningKey::from_slice(&bytes).map_err(|_| WRONG)?;
    Ok(bytes)
}

/// `HUB_PUSH_CONTACT`: who a push service may write to about this hub's
/// pushes, a `mailto:` or `https://` address.
pub fn contact(value: &str) -> anyhow::Result<String> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(DEFAULT_CONTACT.to_string());
    }
    let fine = v.len() <= 200
        && !v.bytes().any(|b| b.is_ascii_control() || b.is_ascii_whitespace() || b == b'"' || b == b'\\')
        && (v.strip_prefix("mailto:").is_some_and(|m| m.contains('@')) || v.starts_with("https://"));
    if !fine {
        anyhow::bail!("HUB_PUSH_CONTACT must be a mailto: or https:// address of at most 200 characters");
    }
    Ok(v.to_string())
}

/// The `Authorization` header of one push (RFC 8292, `vapid`): a token
/// signed with the subscription's own key, naming the push service
/// (`aud`, the endpoint's origin), an end (`exp`) and a contact (`sub`).
/// None: the key or the endpoint is not what registration checked.
pub fn authorization(endpoint: &reqwest::Url, key: &[u8], contact: &str, now_secs: i64) -> Option<String> {
    let key = SigningKey::from_slice(key).ok()?;
    let aud = endpoint.origin().ascii_serialization();
    let head = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = URL_SAFE_NO_PAD.encode(json!({ "aud": aud, "exp": now_secs + VAPID_VALID_SECS, "sub": contact }).to_string());
    let signed = format!("{head}.{claims}");
    let signature: Signature = key.sign(signed.as_bytes());
    let public = key.verifying_key().to_encoded_point(false);
    Some(format!("vapid t={signed}.{}, k={}", URL_SAFE_NO_PAD.encode(signature.to_bytes()), URL_SAFE_NO_PAD.encode(public.as_bytes())))
}

/// What a notification's JSON holds, for tests.
pub fn parsed(payload: &str) -> Value {
    serde_json::from_str(payload).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{signature::Verifier, VerifyingKey};

    fn note(from: &str, name: &str, text: &str, files: &[&str]) -> Note {
        Note { from: from.into(), name: name.into(), text: text.into(), files: files.iter().map(|f| f.to_string()).collect() }
    }

    const APP: &str = "https://pc.example.ts.net/";

    #[test]
    fn what_a_notification_says() {
        // one message
        let p = parsed(&payload(&[note("ann.3f9c2a", "Ann Example", "Are we still on for 3?", &[])], APP, true, 3));
        assert_eq!(
            p,
            json!({"web_push": 8030, "notification": {"title": "Ann Example", "body": "Are we still on for 3?",
                   "navigate": "https://pc.example.ts.net/#/chat/ann.3f9c2a", "tag": "hc-ann.3f9c2a", "silent": false}, "app_badge": 3})
        );
        // several from one sender: the newest, and how many more
        let p = parsed(&payload(&[note("ann.3f9c2a", "Ann", "third", &[]), note("ann.3f9c2a", "Ann", "second", &[]), note("ann.3f9c2a", "Ann", "first", &[])], APP, true, 3));
        assert_eq!(p["notification"]["title"], "Ann");
        assert_eq!(p["notification"]["body"], "third (+2 more)");
        assert_eq!(p["notification"]["tag"], "hc-ann.3f9c2a");
        // several senders: names newest first, at most two
        let many = [note("k.111111", "Kim", "c", &[]), note("ann.3f9c2a", "Ann", "b", &[]), note("k.111111", "Kim", "a", &[]), note("bo.222222", "", "z", &[]), note("cy.333333", "Cy", "y", &[])];
        let p = parsed(&payload(&many, APP, true, 5));
        assert_eq!(p["notification"]["title"], "5 new messages");
        assert_eq!(p["notification"]["body"], "From Kim, Ann and 2 others");
        assert_eq!(p["notification"]["navigate"], APP);
        assert_eq!(p["notification"]["tag"], "hc-many");
        let p = parsed(&payload(&many[..2], APP, true, 2));
        assert_eq!(p["notification"]["body"], "From Kim and Ann");
        let p = parsed(&payload(&many[..4], APP, true, 4));
        assert_eq!(p["notification"]["body"], "From Kim, Ann and 1 other");
        // no name: the address
        let p = parsed(&payload(&[note("bo.222222", " ", "hi", &[])], APP, true, 1));
        assert_eq!(p["notification"]["title"], "bo.222222");
        // files only
        let p = parsed(&payload(&[note("ann.3f9c2a", "Ann", "", &["plan.pdf"])], APP, true, 1));
        assert_eq!(p["notification"]["body"], "Sent a file: plan.pdf");
        let p = parsed(&payload(&[note("ann.3f9c2a", "Ann", " ", &["a", "b", "c"])], APP, true, 1));
        assert_eq!(p["notification"]["body"], "Sent 3 files");
    }

    #[test]
    fn without_the_text_nothing_of_the_mail_is_in_it() {
        let secret = [note("ann.3f9c2a", "Ann Example", "the secret plan", &["secret.pdf"])];
        let one = payload(&secret, APP, false, 1);
        assert_eq!(parsed(&one)["notification"]["title"], "Hubchat");
        assert_eq!(parsed(&one)["notification"]["body"], "New message");
        assert!(!one.contains("secret") && !one.contains("Ann Example") && !one.contains("ann.3f9c2a"), "{one}");
        assert_eq!(parsed(&one)["notification"]["navigate"], APP);
        assert_eq!(parsed(&one)["notification"]["tag"], "hc-new");
        let two = payload(&[note("ann.3f9c2a", "Ann", "x", &[]), note("bo.222222", "Bo", "y", &[])], APP, false, 2);
        assert_eq!(parsed(&two)["notification"]["body"], "2 new messages");
        assert!(!two.contains("Bo"), "{two}");
    }

    #[test]
    fn it_is_cut_to_fit() {
        assert_eq!(cut("short", 80), "short");
        assert_eq!(cut("one\ntwo\r\n  three", 80), "one two three");
        let long = "é".repeat(500);
        let c = cut(&long, 160);
        assert_eq!(c.chars().count(), 160);
        assert!(c.ends_with('…'));
        assert_eq!(cut("word  and a gap here", 6), "word…");
        let p = parsed(&payload(&[note("ann.3f9c2a", &"N".repeat(200), &"t".repeat(900), &[]), note("ann.3f9c2a", "", "older", &[])], APP, true, 5000));
        assert_eq!(p["notification"]["title"].as_str().unwrap().chars().count(), TITLE_MAX);
        let body = p["notification"]["body"].as_str().unwrap();
        assert_eq!(body.chars().count(), BODY_MAX);
        assert!(body.ends_with("… (+1 more)"), "{body}");
        assert_eq!(p["app_badge"], 999);
        // four-byte characters and the longest address: still under the limit
        let wide = payload(&[note(&"s".repeat(64), &"𝒜".repeat(300), &"𝒷".repeat(900), &[])], &format!("https://{}.example/", "h".repeat(280)), true, 1);
        assert!(wide.len() <= PAYLOAD_MAX, "{}", wide.len());
        assert!(parsed(&wide)["notification"]["body"].as_str().unwrap().ends_with('…'));
    }

    #[test]
    fn the_app_address_and_the_contact() {
        assert_eq!(navigate("https://pc.example.ts.net", false).unwrap(), "https://pc.example.ts.net/");
        assert_eq!(navigate("https://pc.example.ts.net:8443/chat/", false).unwrap(), "https://pc.example.ts.net:8443/chat/");
        assert_eq!(navigate("http://localhost:8080/", true).unwrap(), "http://localhost:8080/");
        for bad in [
            "http://localhost:8080/",
            "http://pc.example.ts.net/",
            "https://user:pw@pc.example/",
            "https://pc.example/?x=1",
            "https://pc.example/#frag",
            "javascript:alert(1)",
            "pc.example",
            "https://pc.example/a b",
            "",
        ] {
            assert!(navigate(bad, false).is_err(), "{bad}");
        }
        assert!(navigate("http://example.com/", true).is_err());
        assert!(navigate(&format!("https://pc.example/{}", "a".repeat(300)), false).is_err());
        assert_eq!(contact("").unwrap(), DEFAULT_CONTACT);
        assert_eq!(contact(" mailto:owner@example.com ").unwrap(), "mailto:owner@example.com");
        assert_eq!(contact("https://example.com/contact").unwrap(), "https://example.com/contact");
        for bad in ["owner@example.com", "mailto:nobody", "http://example.com", "mailto:a@b.c\"x"] {
            assert!(contact(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_signature_is_the_subscriptions_own() {
        let key = [7u8; 32];
        let text = URL_SAFE_NO_PAD.encode(key);
        assert_eq!(vapid_key(&text).unwrap(), key);
        for bad in ["", "short", &URL_SAFE_NO_PAD.encode([0u8; 32]), &URL_SAFE_NO_PAD.encode([255u8; 32]), &format!("{text}A")] {
            assert!(vapid_key(bad).is_err(), "{bad}");
        }
        let endpoint = reqwest::Url::parse("https://web.push.apple.com/QGk3-secret").unwrap();
        let header = authorization(&endpoint, &key, "mailto:owner@example.com", 1_800_000_000).unwrap();
        let (token, public) = header.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(URL_SAFE_NO_PAD.decode(parts[0]).unwrap(), br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims, json!({"aud": "https://web.push.apple.com", "exp": 1_800_000_000i64 + 12 * 3600, "sub": "mailto:owner@example.com"}));
        // the key in the header is the registered one's public half, and it signed the token
        let verifying = VerifyingKey::from_sec1_bytes(&URL_SAFE_NO_PAD.decode(public).unwrap()).unwrap();
        assert_eq!(verifying, *SigningKey::from_slice(&key).unwrap().verifying_key());
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        verifying.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature).unwrap();
        // a port that is not the scheme's own is part of the audience
        let local = authorization(&reqwest::Url::parse("http://127.0.0.1:4455/x").unwrap(), &key, DEFAULT_CONTACT, 0).unwrap();
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(local.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "http://127.0.0.1:4455");
        assert!(authorization(&endpoint, &[0u8; 32], DEFAULT_CONTACT, 0).is_none());
    }
}
