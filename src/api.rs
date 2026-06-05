use std::time::Duration;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const USER_AGENT: &str = "claude-code/2.1.34";
const OAUTH_BETA: &str = "oauth-2025-04-20";

pub fn fetch_usage(token: &str) -> Option<String> {
    let resp = ureq::get(USAGE_URL)
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", OAUTH_BETA)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?;
    let body = resp.into_string().ok()?;
    // Only return JSON that has a `.five_hour` key — otherwise it's an error/rate-limit response.
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    if v.get("five_hour").is_some() {
        Some(body)
    } else {
        None
    }
}

const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/daniel3303/ClaudeCodeStatusLine/releases/latest";

pub fn fetch_latest_release() -> Option<String> {
    let resp = ureq::get(GITHUB_RELEASES_URL)
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(5))
        .call()
        .ok()?;
    let body = resp.into_string().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    if v.get("tag_name").is_some() {
        Some(body)
    } else {
        None
    }
}
