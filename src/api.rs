use std::time::Duration;

use ureq::Agent;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const USER_AGENT: &str = "claude-code/2.1.34";
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// An agent with a whole-request `timeout`. The proxy environment variables
/// are ignored (ureq 3 would otherwise honour them; ureq 2, which this code
/// was written against, did not).
fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .timeout_global(Some(timeout))
        .proxy(None)
        .build()
        .into()
}

/// Fetch the usage JSON. `cli_version` is the Claude Code version from the
/// session payload; it is sent in the User-Agent, falling back to a fixed one.
pub fn fetch_usage(token: &str, cli_version: Option<&str>) -> Option<String> {
    let user_agent =
        cli_version.map_or_else(|| USER_AGENT.to_string(), |v| format!("claude-code/{v}"));
    let mut resp = agent(Duration::from_secs(10))
        .get(USAGE_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", OAUTH_BETA)
        .header("User-Agent", user_agent)
        .call()
        .ok()?;
    let body = resp.body_mut().read_to_string().ok()?;
    // Only return JSON that has a `.five_hour` key — otherwise it's an error/rate-limit response.
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    if v.get("five_hour").is_some() {
        Some(body)
    } else {
        None
    }
}

const GITHUB_RELEASES_URL: &str = "https://api.github.com/repos/folded/ccstatus-rs/releases/latest";

pub fn fetch_latest_release() -> Option<String> {
    let mut resp = agent(Duration::from_secs(5))
        .get(GITHUB_RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .call()
        .ok()?;
    let body = resp.body_mut().read_to_string().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    if v.get("tag_name").is_some() {
        Some(body)
    } else {
        None
    }
}
