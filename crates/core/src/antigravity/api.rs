use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Value, json};
use switch_protocol::{Identity, UsageWindow};
use ureq::{Agent, Body, http::Response};

use crate::provider::RemoteError;

const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USER_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
/// The installed-app client every copy of Antigravity ships with. Google does not treat its secret as confidential.
const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "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";
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email";
const PRODUCTION: &str = "https://cloudcode-pa.googleapis.com";
/// Has the live counters; production can report untouched windows.
const DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
const LOAD_CODE_ASSIST: &str = "/v1internal:loadCodeAssist";
const QUOTA_SUMMARY: &str = "/v1internal:retrieveUserQuotaSummary";
const UNAVAILABLE: &str = "Antigravity usage is temporarily unavailable";
const NO_SUBSCRIPTION: &str = "No Antigravity subscription on this Google account";

pub struct Api {
    agent: Agent,
}

pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_at_ms: i64,
}

pub struct CodeAssist {
    pub plan: Option<String>,
    pub project: Option<String>,
}

struct Answer {
    status: u16,
    retry_after: Option<Duration>,
    body: Value,
}

impl Answer {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn requires_subscription(&self) -> bool {
        self.status == 403
            && self.body["error"]["details"]
                .as_array()
                .is_some_and(|details| details.iter().any(|detail| detail["reason"] == "SUBSCRIPTION_REQUIRED"))
    }
}

impl Api {
    pub fn new() -> Self {
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .into();
        Self { agent }
    }

    pub fn authorize_url(&self, redirect_uri: &str, state: &str, code_challenge: &str) -> String {
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .finish();
        format!("{AUTHORIZE_URL}?{query}")
    }

    pub fn exchange(&self, code: &str, redirect_uri: &str, code_verifier: &str) -> Result<Tokens, RemoteError> {
        let form = [
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
            ("code_verifier", code_verifier),
        ];
        self.token(form.as_slice(), "signing in")
    }

    pub fn refresh(&self, refresh_token: &str) -> Result<Tokens, RemoteError> {
        let form = [
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ];
        self.token(form.as_slice(), "renewing the login")
    }

    fn token(&self, form: &[(&str, &str)], purpose: &str) -> Result<Tokens, RemoteError> {
        let response = self.agent.post(TOKEN_URL).send_form(form.iter().copied()).map_err(unreachable)?;
        let answer = answer(response);
        match answer.status {
            200..300 => parse_tokens(&answer.body, Timestamp::now()),
            400..=403 if answer.body["error"] == "invalid_grant" => Err(RemoteError::Rejected),
            429 => Err(RemoteError::RateLimited(answer.retry_after)),
            status => Err(RemoteError::Other(format!("Google answered {status} while {purpose}"))),
        }
    }

    pub fn user(&self, access_token: &str) -> Result<Identity, RemoteError> {
        let response = self
            .agent
            .get(USER_URL)
            .header("authorization", format!("Bearer {access_token}"))
            .call()
            .map_err(unreachable)?;
        let answer = answer(response);
        match answer.status {
            200..300 => parse_user(&answer.body),
            401 | 403 => Err(RemoteError::Unauthorized),
            429 => Err(RemoteError::RateLimited(answer.retry_after)),
            status => Err(RemoteError::Other(format!("Google answered {status}"))),
        }
    }

    /// Asks with Antigravity's client metadata; without it the answer has no tier to read the plan from.
    pub fn code_assist(&self, access_token: &str) -> Result<CodeAssist, RemoteError> {
        let body =
            json!({"metadata": {"ideType": "ANTIGRAVITY", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI"}});
        let mut answers = Vec::new();
        for base in [PRODUCTION, DAILY] {
            let Ok(answer) = self.post(base, LOAD_CODE_ASSIST, access_token, &body) else { continue };
            if answer.ok() {
                return Ok(parse_code_assist(&answer.body));
            }
            let refused = matches!(answer.status, 401 | 403);
            answers.push(answer);
            if refused {
                break;
            }
        }
        Err(failure(&answers))
    }

    pub fn quota(&self, access_token: &str, project: Option<&str>) -> Result<Vec<UsageWindow>, RemoteError> {
        let body = project.map_or_else(|| json!({}), |project| json!({"project": project}));
        let mut answers = Vec::new();
        for base in [DAILY, PRODUCTION] {
            let Ok(answer) = self.post(base, QUOTA_SUMMARY, access_token, &body) else { continue };
            let complete = base == DAILY && answer.ok() && is_complete(&answer.body);
            answers.push(answer);
            if complete {
                break;
            }
        }
        let bodies: Vec<&Value> = answers.iter().filter(|answer| answer.ok()).map(|answer| &answer.body).collect();
        if bodies.is_empty() { Err(failure(&answers)) } else { parse_quota(&bodies) }
    }

    fn post(&self, base: &str, path: &str, access_token: &str, body: &Value) -> Result<Answer, RemoteError> {
        let response = self
            .agent
            .post(format!("{base}{path}"))
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {access_token}"))
            .header("user-agent", user_agent())
            .send_json(body)
            .map_err(unreachable)?;
        Ok(answer(response))
    }
}

/// The hub client identity is the one that is told about the separate Argon quota.
fn user_agent() -> String {
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "amd64" };
    format!("antigravity/hub/2.24.0 (aidev_client; os_type=darwin; arch={arch})")
}

fn answer(response: Response<Body>) -> Answer {
    let status = response.status().as_u16();
    let retry_after = retry_after(&response);
    let body = response.into_body().read_json().unwrap_or_default();
    Answer { status, retry_after, body }
}

fn failure(answers: &[Answer]) -> RemoteError {
    if answers.is_empty() {
        return RemoteError::Other("could not reach Antigravity".to_owned());
    }
    if answers.iter().any(|answer| answer.status == 401) {
        return RemoteError::Unauthorized;
    }
    if answers.iter().any(Answer::requires_subscription) {
        return RemoteError::Other(NO_SUBSCRIPTION.to_owned());
    }
    if answers.iter().any(|answer| answer.status == 403) {
        return RemoteError::Unauthorized;
    }
    match answers.iter().find(|answer| answer.status == 429) {
        Some(limited) => RemoteError::RateLimited(limited.retry_after),
        None => RemoteError::Other(UNAVAILABLE.to_owned()),
    }
}

fn unreachable(_: ureq::Error) -> RemoteError {
    RemoteError::Other("could not reach Google".to_owned())
}

fn unexpected() -> RemoteError {
    RemoteError::Other("Google sent an unexpected answer".to_owned())
}

fn retry_after(response: &Response<Body>) -> Option<Duration> {
    let seconds = response.headers().get("retry-after")?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

fn parse_tokens(body: &Value, now: Timestamp) -> Result<Tokens, RemoteError> {
    let access = body["access_token"].as_str().filter(|token| !token.is_empty()).ok_or_else(unexpected)?;
    let lifetime_seconds = body["expires_in"].as_i64().unwrap_or(3600);
    Ok(Tokens {
        access: access.to_owned(),
        refresh: body["refresh_token"].as_str().filter(|token| !token.is_empty()).map(str::to_owned),
        expires_at_ms: now.as_millisecond() + lifetime_seconds * 1000,
    })
}

fn parse_user(body: &Value) -> Result<Identity, RemoteError> {
    let text = |value: &Value| value.as_str().filter(|text| !text.is_empty()).map(str::to_owned);
    let email = text(&body["email"]);
    let account_id = text(&body["id"]).or_else(|| email.clone()).ok_or_else(unexpected)?;
    Ok(Identity { account_id, email: email.unwrap_or_default(), organization_id: None, organization: None })
}

fn parse_code_assist(body: &Value) -> CodeAssist {
    let paid = &body["paidTier"];
    let plan = match paid["id"].as_str() {
        Some("g1-pro-tier") => Some("Google AI Pro".to_owned()),
        Some("g1-ultra-tier") => Some("Google AI Ultra".to_owned()),
        Some("g1-ultra-lite-tier") => Some("Google AI Ultra Lite".to_owned()),
        _ => paid["name"].as_str().or(body["currentTier"]["name"].as_str()).and_then(format_plan),
    };
    let project = body["cloudaicompanionProject"].as_str().filter(|project| !project.is_empty()).map(str::to_owned);
    CodeAssist { plan, project }
}

fn format_plan(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    if let Some(tier) = ["Ultra Lite", "Ultra", "Pro"].iter().find(|tier| lower.contains(&tier.to_lowercase())) {
        return Some(format!("Google AI {tier}"));
    }
    let words: Vec<String> = trimmed
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect()
            })
        })
        .collect();
    Some(words.join(" "))
}

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
const ARGON_GROUP: &str = "Gemini 4 Argon";

struct Bucket {
    id: &'static str,
    label: &'static str,
    period: i64,
    primary: bool,
}

const BUCKETS: [Bucket; 4] = [
    Bucket { id: "gemini-5h", label: "Gemini 5h", period: 5 * HOUR, primary: true },
    Bucket { id: "gemini-weekly", label: "Gemini 7d", period: 7 * DAY, primary: true },
    Bucket { id: "3p-5h", label: "Claude 5h", period: 5 * HOUR, primary: false },
    Bucket { id: "3p-weekly", label: "Claude 7d", period: 7 * DAY, primary: false },
];

fn groups(body: &Value) -> Option<&Vec<Value>> {
    body["response"]["groups"].as_array().or_else(|| body["groups"].as_array())
}

fn is_argon(group: &Value) -> bool {
    group["displayName"] == ARGON_GROUP
}

fn known(id: &str) -> bool {
    BUCKETS.iter().any(|bucket| bucket.id == id)
}

/// Google leaves zero values out of its JSON, so a bucket with a reset time but no fraction is used up.
fn fraction(bucket: &Value) -> Option<f64> {
    match &bucket["remainingFraction"] {
        Value::Null if timestamp(&bucket["resetTime"]).is_some() => Some(0.0),
        value => value.as_f64().filter(|fraction| fraction.is_finite()),
    }
}

fn shared_buckets(groups: &[Value]) -> impl Iterator<Item = &Value> {
    groups.iter().filter(|group| !is_argon(group)).flat_map(|group| group["buckets"].as_array().into_iter().flatten())
}

/// The daily answer is enough when it measures every shared bucket.
fn is_complete(body: &Value) -> bool {
    let Some(groups) = groups(body) else { return false };
    let measured: Vec<&str> = shared_buckets(groups)
        .filter(|bucket| fraction(bucket).is_some())
        .filter_map(|bucket| bucket["bucketId"].as_str())
        .collect();
    BUCKETS.iter().all(|bucket| measured.contains(&bucket.id))
}

/// Buckets seen first win, so the daily answer is preferred over production.
fn parse_quota(bodies: &[&Value]) -> Result<Vec<UsageWindow>, RemoteError> {
    let group_lists: Vec<&Vec<Value>> = bodies.iter().filter_map(|body| groups(body)).collect();
    if group_lists.is_empty() {
        return Err(unexpected());
    }
    let mut pooled: Vec<(&str, f64, Option<Timestamp>)> = Vec::new();
    for bucket in group_lists.iter().flat_map(|groups| shared_buckets(groups)) {
        let Some(id) = bucket["bucketId"].as_str().filter(|id| known(id)) else { continue };
        if pooled.iter().any(|(seen, ..)| *seen == id) {
            continue;
        }
        if let Some(fraction) = fraction(bucket) {
            pooled.push((id, fraction, timestamp(&bucket["resetTime"])));
        }
    }
    let mut windows: Vec<UsageWindow> = BUCKETS
        .iter()
        .filter_map(|bucket| {
            let (_, fraction, resets_at) = pooled.iter().find(|(id, ..)| *id == bucket.id)?;
            Some(window(bucket.id, bucket.label, *fraction, *resets_at, bucket.period, bucket.primary))
        })
        .collect();
    if let Some(argon) = group_lists.iter().find_map(|groups| argon_window(groups)) {
        let index = windows.iter().position(|window| window.id.starts_with("3p-")).unwrap_or(windows.len());
        windows.insert(index, argon);
    }
    if windows.is_empty() { Err(unexpected()) } else { Ok(windows) }
}

/// The Argon group has its own weekly bucket, read only when there is exactly one usable one.
fn argon_window(groups: &[Value]) -> Option<UsageWindow> {
    let buckets = groups
        .iter()
        .filter(|group| is_argon(group))
        .flat_map(|group| group["buckets"].as_array().into_iter().flatten());
    let weekly: Vec<&Value> = buckets.filter(|bucket| bucket["window"] == "weekly").collect();
    let [bucket] = weekly.as_slice() else { return None };
    let id = bucket["bucketId"].as_str().filter(|id| !id.is_empty())?;
    if known(id) {
        return None;
    }
    match &bucket["disabled"] {
        Value::Null | Value::Bool(false) => {}
        _ => return None,
    }
    let fraction = fraction(bucket)?;
    Some(window("argon-weekly", "Argon 7d", fraction, timestamp(&bucket["resetTime"]), 7 * DAY, false))
}

fn window(
    id: &str,
    label: &str,
    fraction: f64,
    resets_at: Option<Timestamp>,
    period: i64,
    primary: bool,
) -> UsageWindow {
    UsageWindow {
        id: id.to_owned(),
        label: label.to_owned(),
        used_percent: Some((1.0 - fraction.clamp(0.0, 1.0)) * 100.0),
        resets_at,
        primary,
        period_seconds: Some(period),
    }
}

fn timestamp(value: &Value) -> Option<Timestamp> {
    value.as_str()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(id: &str, fraction: f64) -> Value {
        json!({"bucketId": id, "remainingFraction": fraction, "resetTime": "2026-11-25T18:00:00Z"})
    }

    fn summary(buckets: Vec<Value>) -> Value {
        json!({"response": {"groups": [{"displayName": "Gemini", "buckets": buckets}]}})
    }

    fn labels(windows: &[UsageWindow]) -> Vec<(&str, Option<f64>, bool)> {
        windows.iter().map(|window| (window.label.as_str(), window.used_percent, window.primary)).collect()
    }

    #[test]
    fn a_complete_summary_reads_every_bucket() {
        let body = summary(vec![
            bucket("3p-weekly", 1.0),
            bucket("gemini-5h", 0.75),
            bucket("gemini-weekly", 0.5),
            bucket("3p-5h", 0.0),
        ]);
        assert!(is_complete(&body));
        let windows = parse_quota(&[&body]).unwrap();
        assert_eq!(
            labels(&windows),
            [
                ("Gemini 5h", Some(25.0), true),
                ("Gemini 7d", Some(50.0), true),
                ("Claude 5h", Some(100.0), false),
                ("Claude 7d", Some(0.0), false),
            ]
        );
        assert_eq!(windows[0].period_seconds, Some(5 * HOUR));
        assert_eq!(windows[1].period_seconds, Some(7 * DAY));
        assert!(windows[0].resets_at.is_some());
    }

    #[test]
    fn missing_buckets_come_from_the_next_answer_and_the_first_wins() {
        let daily = summary(vec![bucket("gemini-5h", 0.9), json!({"bucketId": "gemini-weekly"})]);
        let production = json!({"groups": [{"buckets": [bucket("gemini-5h", 0.1), bucket("gemini-weekly", 0.6)]}]});
        assert!(!is_complete(&daily));
        let windows = parse_quota(&[&daily, &production]).unwrap();
        let used: Vec<_> = windows.iter().map(|window| window.used_percent.unwrap().round()).collect();
        assert_eq!(used, [10.0, 40.0]);
    }

    #[test]
    fn a_bucket_with_a_reset_time_but_no_fraction_is_used_up() {
        let daily = summary(vec![json!({"bucketId": "gemini-5h", "resetTime": "2026-11-25T18:00:00Z"})]);
        let production = summary(vec![bucket("gemini-5h", 1.0)]);
        let windows = parse_quota(&[&daily, &production]).unwrap();
        assert_eq!(labels(&windows), [("Gemini 5h", Some(100.0), true)]);
    }

    #[test]
    fn unknown_buckets_are_skipped_and_fractions_are_clamped() {
        let body = summary(vec![bucket("something-new", 0.2), bucket("gemini-5h", 1.5), bucket("3p-5h", -1.0)]);
        assert_eq!(
            labels(&parse_quota(&[&body]).unwrap()),
            [("Gemini 5h", Some(0.0), true), ("Claude 5h", Some(100.0), false)]
        );
        assert!(parse_quota(&[&summary(vec![bucket("something-new", 0.2)])]).is_err());
        assert!(parse_quota(&[&json!({"unexpected": true})]).is_err());
    }

    #[test]
    fn the_argon_weekly_bucket_comes_before_the_claude_windows() {
        let mut body = summary(vec![bucket("gemini-5h", 1.0), bucket("3p-5h", 1.0)]);
        let argon = json!({"displayName": ARGON_GROUP, "buckets": [
            {"bucketId": "argon-weekly-x", "remainingFraction": 0.25, "window": "weekly"},
            {"bucketId": "argon-daily", "remainingFraction": 0.5, "window": "daily"}
        ]});
        body["response"]["groups"].as_array_mut().unwrap().push(argon);
        let windows = parse_quota(&[&body]).unwrap();
        assert_eq!(
            labels(&windows),
            [("Gemini 5h", Some(0.0), true), ("Argon 7d", Some(75.0), false), ("Claude 5h", Some(0.0), false)]
        );
    }

    #[test]
    fn an_unusable_argon_bucket_is_left_out() {
        let argon = |buckets: Value| json!({"groups": [{"displayName": ARGON_GROUP, "buckets": buckets}]});
        let weekly = |extra: Value| {
            let mut bucket = json!({"bucketId": "argon-weekly-x", "remainingFraction": 0.5, "window": "weekly"});
            bucket.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            bucket
        };
        assert!(argon_window(groups(&argon(json!([weekly(json!({}))]))).unwrap()).is_some());
        for buckets in [
            json!([weekly(json!({"disabled": true}))]),
            json!([weekly(json!({"disabled": "no"}))]),
            json!([weekly(json!({"remainingFraction": null}))]),
            json!([weekly(json!({"bucketId": "gemini-weekly"}))]),
            json!([weekly(json!({})), weekly(json!({"bucketId": "argon-other"}))]),
        ] {
            assert!(argon_window(groups(&argon(buckets.clone())).unwrap()).is_none(), "{buckets}");
        }
    }

    #[test]
    fn the_plan_comes_from_the_paid_tier() {
        let plan = |body: Value| parse_code_assist(&body).plan;
        assert_eq!(
            plan(json!({"paidTier": {"id": "g1-pro-tier", "name": "whatever"}})).as_deref(),
            Some("Google AI Pro")
        );
        assert_eq!(plan(json!({"paidTier": {"id": "g1-ultra-tier"}})).as_deref(), Some("Google AI Ultra"));
        assert_eq!(plan(json!({"paidTier": {"id": "g1-ultra-lite-tier"}})).as_deref(), Some("Google AI Ultra Lite"));
        assert_eq!(
            plan(json!({"paidTier": {"id": "x", "name": "google ai ultra lite"}})).as_deref(),
            Some("Google AI Ultra Lite")
        );
        assert_eq!(plan(json!({"currentTier": {"name": "free TIER"}})).as_deref(), Some("Free Tier"));
        assert_eq!(plan(json!({"currentTier": {"name": "  "}})), None);
        let project = parse_code_assist(&json!({"cloudaicompanionProject": "p-1"})).project;
        assert_eq!(project.as_deref(), Some("p-1"));
    }

    fn answer(status: u16, body: Value) -> Answer {
        Answer { status, retry_after: Some(Duration::from_secs(7)), body }
    }

    #[test]
    fn failures_are_told_apart() {
        let subscription = json!({"error": {"details": [{"reason": "SUBSCRIPTION_REQUIRED"}]}});
        assert!(matches!(failure(&[answer(401, json!({}))]), RemoteError::Unauthorized));
        assert!(matches!(failure(&[answer(403, json!({}))]), RemoteError::Unauthorized));
        assert!(matches!(
            failure(&[answer(403, subscription.clone()), answer(403, json!({}))]),
            RemoteError::Other(message) if message == NO_SUBSCRIPTION
        ));
        assert!(matches!(failure(&[answer(403, subscription), answer(401, json!({}))]), RemoteError::Unauthorized));
        assert!(matches!(
            failure(&[answer(500, json!({})), answer(429, json!({}))]),
            RemoteError::RateLimited(Some(after)) if after.as_secs() == 7
        ));
        assert!(matches!(failure(&[answer(500, json!({}))]), RemoteError::Other(message) if message == UNAVAILABLE));
        assert!(matches!(failure(&[]), RemoteError::Other(_)));
    }

    #[test]
    fn tokens_and_users_are_read() {
        let now = Timestamp::from_second(1000).unwrap();
        let tokens = parse_tokens(&json!({"access_token": "a", "expires_in": 10}), now).unwrap();
        assert_eq!(tokens.expires_at_ms, 1_010_000);
        assert!(tokens.refresh.is_none());
        assert!(parse_tokens(&json!({"access_token": ""}), now).is_err());
        let user = parse_user(&json!({"id": "123", "email": "a@example.com"})).unwrap();
        assert_eq!((user.account_id.as_str(), user.email.as_str()), ("123", "a@example.com"));
        assert_eq!(parse_user(&json!({"email": "a@example.com"})).unwrap().account_id, "a@example.com");
        assert!(parse_user(&json!({})).is_err());
    }

    #[test]
    fn the_authorize_url_asks_for_offline_access_with_pkce() {
        let url = Api::new().authorize_url("http://127.0.0.1:51121/oauth-callback", "s1", "c1");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?client_id=1071006060591-"));
        for part in [
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A51121%2Foauth-callback",
            "scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.email",
            "code_challenge=c1",
            "code_challenge_method=S256",
            "state=s1",
            "access_type=offline",
            "prompt=consent",
        ] {
            assert!(url.contains(part), "{part} in {url}");
        }
    }
}
