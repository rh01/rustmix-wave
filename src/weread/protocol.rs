//! Request builders for the WeRead web reader and the official agent gateway.

use crate::weread::{
    crypto::{self, e_hash, sign, sorted_query, web_app_id, READER_TOKEN, USER_AGENT},
    limits::MAX_FIELD_CHARS,
};

pub const WEB_ORIGIN: &str = "https://weread.qq.com";
pub const OFFICIAL_GATEWAY: &str = "https://i.weread.qq.com/api/agent/gateway";
pub const SKILL_VERSION: &str = "1.0.4";

pub const RENEWAL_BODY: &str = r#"{"rq":"%2Fweb%2Fbook%2Fread","ql":false}"#;

#[must_use]
pub fn login_uid_url() -> String {
    format!("{WEB_ORIGIN}/api/auth/getLoginUid")
}

#[must_use]
pub fn login_info_url(uid: &str) -> String {
    format!(
        "{WEB_ORIGIN}/api/auth/getLoginInfo?uid={}&otp=",
        crypto::url_encode(uid)
    )
}

#[must_use]
pub fn confirm_url(uid: &str) -> String {
    format!("{WEB_ORIGIN}/web/confirm?uid={}", crypto::url_encode(uid))
}

#[must_use]
pub fn skills_page_url() -> String {
    format!("{WEB_ORIGIN}/r/weread-skills")
}

#[must_use]
pub fn shelf_url() -> String {
    format!("{WEB_ORIGIN}/web/shelf/sync")
}

#[must_use]
pub fn book_info_url(book_id: &str) -> String {
    format!(
        "{WEB_ORIGIN}/web/book/info?bookId={}",
        crypto::url_encode(book_id)
    )
}

#[must_use]
pub fn chapter_infos_body(book_id: &str) -> String {
    format!("{{\"bookIds\":[\"{}\"]}}", json_escape(book_id))
}

#[must_use]
pub fn reader_url(book_id: &str, chapter_uid: Option<&str>) -> String {
    let mut url = format!("{WEB_ORIGIN}/web/reader/{}", e_hash(book_id));
    if let Some(uid) = chapter_uid.filter(|uid| !uid.is_empty()) {
        url.push('k');
        url.push_str(&e_hash(uid));
    }
    url
}

#[must_use]
pub fn reader_path(book_id: &str, chapter_uid: Option<&str>) -> String {
    let mut path = format!("/web/reader/{}", e_hash(book_id));
    if let Some(uid) = chapter_uid.filter(|uid| !uid.is_empty()) {
        path.push('k');
        path.push_str(&e_hash(uid));
    }
    path
}

#[must_use]
pub fn content_json(
    book_id: &str,
    chapter_uid: &str,
    psvts: &str,
    mut ct: u64,
    random_square: u64,
    style: bool,
) -> String {
    if e_hash(&ct.to_string()) == psvts {
        ct = ct.saturating_add(1);
    }
    let fields = [
        ("b", e_hash(book_id)),
        ("c", e_hash(chapter_uid)),
        ("r", random_square.to_string()),
        ("ct", ct.to_string()),
        ("ps", psvts.to_string()),
        ("pc", e_hash(&ct.to_string())),
        ("sc", "1".to_string()),
        ("prevChapter", "false".to_string()),
        ("st", if style { "1" } else { "0" }.to_string()),
    ];
    signed_json(&fields, &["ct", "sc", "st", "r"])
}

#[must_use]
pub fn progress_json(
    book_id: &str,
    chapter_uid: &str,
    chapter_idx: u32,
    chapter_offset: u32,
    summary: &str,
    progress: u8,
    psvts: &str,
    ct: u64,
    ts: u64,
    rn: u32,
) -> String {
    let summary = summary.chars().take(20).collect::<String>();
    let pc = e_hash(&ct.to_string());
    let sg = crypto::sha256_hex(format!("{ts}{rn}{READER_TOKEN}").as_bytes());
    let fields = [
        ("appId", web_app_id(USER_AGENT)),
        ("b", e_hash(book_id)),
        ("c", e_hash(chapter_uid)),
        ("ci", chapter_idx.to_string()),
        ("co", chapter_offset.to_string()),
        ("sm", summary),
        ("pr", progress.to_string()),
        ("rt", "0".to_string()),
        ("ts", ts.to_string()),
        ("rn", rn.to_string()),
        ("sg", sg),
        ("ct", ct.to_string()),
        ("ps", psvts.to_string()),
        ("pc", pc),
    ];
    signed_json(&fields, &["ci", "co", "pr", "rt", "ts", "rn", "ct"])
}

#[must_use]
pub fn gateway_body(api_name: &str, fields: &[(&str, &str)]) -> String {
    let mut body = String::from("{");
    body.push_str(&format!("\"api_name\":\"{}\"", json_escape(api_name)));
    for (key, value) in fields {
        body.push(',');
        body.push('"');
        body.push_str(&json_escape(key));
        body.push_str("\":\"");
        body.push_str(&json_escape(value));
        body.push('"');
    }
    body.push_str(&format!(",\"skill_version\":\"{SKILL_VERSION}\"}}"));
    body
}

#[must_use]
pub fn browser_headers(cookie: &str, referer: &str, json: bool) -> Vec<(String, String)> {
    let mut headers = vec![
        ("User-Agent".into(), USER_AGENT.into()),
        (
            "Accept".into(),
            if json {
                "application/json, text/plain, */*".into()
            } else {
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".into()
            },
        ),
        ("Origin".into(), WEB_ORIGIN.into()),
        ("Referer".into(), referer.into()),
    ];
    if !cookie.is_empty() {
        headers.push(("Cookie".into(), cookie.into()));
    }
    if json {
        headers.push((
            "Content-Type".into(),
            "application/json;charset=UTF-8".into(),
        ));
    }
    headers
}

#[must_use]
pub fn official_headers(api_key: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".into(), format!("Bearer {api_key}")),
        ("Content-Type".into(), "application/json".into()),
        ("Accept".into(), "application/json".into()),
        ("User-Agent".into(), USER_AGENT.into()),
    ]
}

pub fn json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars().take(MAX_FIELD_CHARS) {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn signed_json(fields: &[(&str, String)], numeric: &[&str]) -> String {
    let refs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let signature = sign(&sorted_query(&refs));
    let mut body = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            body.push(',');
        }
        body.push('"');
        body.push_str(key);
        body.push_str("\":");
        if numeric.contains(key) && is_json_number(value) {
            body.push_str(value);
        } else {
            body.push('"');
            body.push_str(&json_escape(value));
            body.push('"');
        }
    }
    body.push_str(",\"s\":\"");
    body.push_str(&signature);
    body.push_str("\"}");
    body
}

fn is_json_number(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

#[must_use]
pub fn extract_psvts(html: &str) -> Option<String> {
    extract_quoted_after(html, "\"psvts\"")
        .or_else(|| extract_quoted_after(html, "'psvts'"))
        .or_else(|| extract_quoted_after(html, "psvts"))
        .filter(|value| !value.is_empty() && value.len() <= 128)
}

fn extract_quoted_after(html: &str, needle: &str) -> Option<String> {
    let start = html.find(needle)?;
    let rest = &html[start + needle.len()..];
    let colon = rest.find(':')?;
    let mut tail = rest[colon + 1..].trim_start();
    let quote = tail.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    tail = &tail[quote.len_utf8()..];
    let end = tail.find(quote)?;
    Some(tail[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::{content_json, extract_psvts, gateway_body, progress_json, reader_url};
    use crate::weread::crypto::{e_hash, sign, sorted_query};

    #[test]
    fn content_body_is_signed_and_uses_numeric_counters() {
        let body = content_json("43208843", "2", "ps-token", 1_700_000_000, 16, false);
        assert!(body.contains("\"st\":0"));
        assert!(body.contains("\"sc\":1"));
        assert!(body.contains("\"prevChapter\":\"false\""));
        assert!(body.contains(&format!("\"b\":\"{}\"", e_hash("43208843"))));
        let signature = body
            .split("\"s\":\"")
            .nth(1)
            .and_then(|rest| rest.strip_suffix("\"}"))
            .unwrap();
        assert_eq!(signature.len() > 0, true);
        let _ = (sign, sorted_query);
    }

    #[test]
    fn reader_url_uses_hashed_ids() {
        assert_eq!(
            reader_url("43208843", None),
            "https://weread.qq.com/web/reader/c9c321c07293508bc9c79df"
        );
    }

    #[test]
    fn psvts_extract_rejects_unterminated_values() {
        assert_eq!(
            extract_psvts(r#"window.__INITIAL_STATE__={"reader":{"psvts":"abc123"}}"#).as_deref(),
            Some("abc123")
        );
        assert_eq!(extract_psvts("psvts: 12"), None);
    }

    #[test]
    fn gateway_body_keeps_parameters_at_the_top_level() {
        let body = gateway_body("/book/bookmarklist", &[("bookId", "43208843")]);
        assert!(body.contains("\"api_name\":\"/book/bookmarklist\""));
        assert!(body.contains("\"bookId\":\"43208843\""));
        assert!(body.contains("\"skill_version\":\"1.0.4\""));
        assert!(!body.contains("\"params\""));
        let _ = progress_json(
            "1",
            "2",
            1,
            0,
            "摘要",
            10,
            "ps",
            1_700_000_000,
            1_700_000_000_123,
            7,
        );
    }
}
