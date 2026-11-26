use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Value, json};
use switch_protocol::{Identity, LimitReset, UsageWindow};
use ureq::{Agent, Body, http::Response};

use crate::provider::{Profile, RedeemError, RedeemOutcome, RemoteError, ResetGrants};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const RESET_GRANTS_URL: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
const RESET_CLAIM_BASE_URL: &str = "https://api.anthropic.com/api/organizations";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const BETA: &str = "oauth-2025-04-20";
const USER_AGENT: &str = "claude-cli/2.1.294 (external, cli)";

pub struct Api {
    agent: Agent,
    claim_agent: Agent,
}

pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_at_ms: i64,
    pub scopes: Option<Vec<String>>,
}

impl Api {
    pub fn new() -> Self {
        let build_agent = |timeout_secs| {
            Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(timeout_secs)))
                .user_agent(USER_AGENT)
                .build()
                .into()
        };
        Self { agent: build_agent(10), claim_agent: build_agent(25) }
    }

    pub fn usage(&self, access_token: &str) -> Result<Vec<UsageWindow>, RemoteError> {
        parse_usage(&self.get(USAGE_URL, access_token)?)
    }

    pub fn reset_grants(&self, access_token: &str) -> Result<ResetGrants, RemoteError> {
        Ok(parse_reset_grants(&self.get(RESET_GRANTS_URL, access_token)?))
    }

    pub fn redeem_reset(
        &self,
        access_token: &str,
        org_id: &str,
        grant_id: &str,
        request_id: &str,
    ) -> Result<RedeemOutcome, RedeemError> {
        if !valid_org_id(org_id) || !valid_grant_id(grant_id) || !valid_request_id(request_id) {
            return Err(RedeemError::Refused("Claude refused the limit reset request.".to_owned()));
        }
        let url = format!("{RESET_CLAIM_BASE_URL}/{org_id}/reset_rate_limits");
        let body = json!({
            "program": "cedar_ember",
            "grant_id": grant_id,
            "request_id": request_id,
        });
        let response = self
            .claim_agent
            .post(&url)
            .header("authorization", format!("Bearer {}", access_token.trim()))
            .header("accept", "application/json")
            .header("anthropic-beta", BETA)
            .send_json(body)
            .map_err(|_| {
                RedeemError::Unsettled(
                    "Could not reach Claude to confirm whether the reset was applied. Refresh in a moment to check."
                        .to_owned(),
                )
            })?;
        let status = response.status().as_u16();
        let retry_after = retry_after(&response);
        let text = response.into_body().read_to_string().unwrap_or_default();
        parse_redeem_response(status, retry_after, &text, Timestamp::now())
    }

    pub fn profile(&self, access_token: &str) -> Result<Profile, RemoteError> {
        parse_profile(&self.get(PROFILE_URL, access_token)?)
    }

    pub fn refresh(&self, refresh_token: &str) -> Result<Tokens, RemoteError> {
        let body = json!({"grant_type": "refresh_token", "refresh_token": refresh_token, "client_id": CLIENT_ID});
        let response = self.agent.post(TOKEN_URL).send_json(body).map_err(unreachable)?;
        let status = response.status().as_u16();
        let retry_after = retry_after(&response);
        let body: Value = response.into_body().read_json().unwrap_or_default();
        match status {
            200..300 => parse_tokens(&body, Timestamp::now()),
            400..=403 if body["error"] == "invalid_grant" => Err(RemoteError::Rejected),
            429 => Err(RemoteError::RateLimited(retry_after)),
            _ => Err(RemoteError::Other(format!("Claude answered {status} while renewing the login"))),
        }
    }

    fn get(&self, url: &str, access_token: &str) -> Result<Value, RemoteError> {
        let response = self
            .agent
            .get(url)
            .header("authorization", format!("Bearer {}", access_token.trim()))
            .header("accept", "application/json")
            .header("anthropic-beta", BETA)
            .call()
            .map_err(unreachable)?;
        match response.status().as_u16() {
            200..300 => response.into_body().read_json().map_err(|_| unexpected()),
            401 => Err(RemoteError::Unauthorized),
            429 => Err(RemoteError::RateLimited(retry_after(&response))),
            status => Err(RemoteError::Other(format!("Claude answered {status}"))),
        }
    }
}

fn unreachable(_: ureq::Error) -> RemoteError {
    RemoteError::Other("could not reach Claude".to_owned())
}

fn unexpected() -> RemoteError {
    RemoteError::Other("Claude sent an unexpected answer".to_owned())
}

fn retry_after(response: &Response<Body>) -> Option<Duration> {
    let seconds = response.headers().get("retry-after")?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

fn parse_tokens(body: &Value, now: Timestamp) -> Result<Tokens, RemoteError> {
    let access = body["access_token"].as_str().ok_or_else(unexpected)?.to_owned();
    let lifetime_seconds = body["expires_in"].as_i64().unwrap_or(3600);
    Ok(Tokens {
        access,
        refresh: body["refresh_token"].as_str().map(str::to_owned),
        expires_at_ms: now.as_millisecond() + lifetime_seconds * 1000,
        scopes: body["scope"].as_str().map(|scope| scope.split_whitespace().map(str::to_owned).collect()),
    })
}

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

fn parse_usage(body: &Value) -> Result<Vec<UsageWindow>, RemoteError> {
    let mut windows = Vec::new();
    for (key, label, period) in [("five_hour", "Session", 5 * HOUR), ("seven_day", "Weekly", 7 * DAY)] {
        let window = &body[key];
        if window.as_object().is_none_or(|window| window.is_empty()) {
            continue;
        }
        let used_percent = match &window["utilization"] {
            Value::Null => None,
            value => Some(value.as_f64().filter(|percent| *percent >= 0.0).ok_or_else(unexpected)?),
        };
        windows.push(UsageWindow {
            id: key.to_owned(),
            label: label.to_owned(),
            used_percent,
            resets_at: timestamp(&window["resets_at"]),
            primary: true,
            period_seconds: Some(period),
        });
    }
    for limit in body["limits"].as_array().into_iter().flatten() {
        let (Some(model), Some(percent)) =
            (limit["scope"]["model"]["display_name"].as_str(), limit["percent"].as_f64())
        else {
            continue;
        };
        windows.push(UsageWindow {
            id: format!("model:{model}"),
            label: model.to_owned(),
            used_percent: Some(percent),
            resets_at: timestamp(&limit["resets_at"]),
            primary: false,
            period_seconds: None,
        });
    }
    if windows.is_empty() { Err(unexpected()) } else { Ok(windows) }
}

fn parse_profile(body: &Value) -> Result<Profile, RemoteError> {
    let text = |value: &Value| value.as_str().filter(|text| !text.is_empty()).map(str::to_owned);
    let organization = &body["organization"];
    Ok(Profile {
        identity: Identity {
            account_id: text(&body["account"]["uuid"]).ok_or_else(unexpected)?,
            email: text(&body["account"]["email"]).unwrap_or_default(),
            organization_id: text(&organization["uuid"]),
            organization: text(&organization["name"]),
        },
        plan: plan(organization),
    })
}

fn plan(organization: &Value) -> Option<String> {
    let name = match organization["organization_type"].as_str() {
        Some("claude_team") => "Team",
        Some("claude_enterprise") => "Enterprise",
        Some("claude_pro") => "Pro",
        _ => match organization["rate_limit_tier"].as_str() {
            Some("default_claude_max_5x") => "Max 5x",
            Some("default_claude_max_20x") => "Max 20x",
            _ => return organization["plan_display_name"].as_str().map(str::to_owned),
        },
    };
    let served = organization["plan_display_name"].as_str().filter(|served| !served.is_empty());
    Some(served.filter(|served| !name.starts_with(served)).unwrap_or(name).to_owned())
}

fn timestamp(value: &Value) -> Option<Timestamp> {
    value.as_str()?.parse().ok()
}

fn flexible_timestamp(value: &Value) -> Option<Timestamp> {
    if let Some(text) = value.as_str().map(str::trim).filter(|text| !text.is_empty()) {
        return text.parse().ok();
    }
    let number = value.as_f64().filter(|number| number.is_finite())?;
    let millis = if number.abs() < 1e10 { number * 1000.0 } else { number };
    Timestamp::from_millisecond(millis.round() as i64).ok()
}

fn valid_org_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn valid_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-')
}

fn valid_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn parse_reset_grants(body: &Value) -> ResetGrants {
    let Some(cedar_ember) = body.get("cedar_ember").and_then(Value::as_object) else {
        return ResetGrants::default();
    };
    if cedar_ember.get("eligible").and_then(Value::as_bool) != Some(true) {
        return ResetGrants::default();
    }
    let mut grants = Vec::new();
    if let Some(entries) = cedar_ember.get("grants").and_then(Value::as_array) {
        for entry in entries {
            let Some(grant) = entry.as_object() else { continue };
            let Some(id) = grant.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) else { continue };
            let Some(left) = grant.get("resets_left").and_then(Value::as_f64).filter(|n| n.is_finite()) else {
                continue;
            };
            let remaining = left.floor().max(0.0) as u32;
            if remaining == 0 {
                continue;
            }
            let clears: Vec<&str> = grant
                .get("clears")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let kind = grant.get("type").and_then(Value::as_str);
            let gives_back_weekly = grant.get("gives_back_weekly").and_then(Value::as_bool) == Some(true);
            let usable_now = grant.get("usable_now").and_then(Value::as_bool) == Some(true);
            let paused = grant.get("paused").and_then(Value::as_bool) == Some(true);
            grants.push(LimitReset {
                id: id.to_owned(),
                label: grant_label(gives_back_weekly, kind, &clears),
                remaining,
                expires_at: grant.get("ends_at").and_then(flexible_timestamp),
                usable: usable_now && !paused,
                detail: grant_detail(&clears, gives_back_weekly),
            });
        }
    }
    let next_grant_id =
        cedar_ember.get("next_grant_id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_owned);
    ResetGrants { resets: grants, next_grant_id }
}

fn grant_label(gives_back_weekly: bool, kind: Option<&str>, clears: &[&str]) -> String {
    if gives_back_weekly {
        return "Fable Reset".to_owned();
    }
    match kind {
        Some("session") => return "Session Reset".to_owned(),
        Some("weekly") => return "Weekly Reset".to_owned(),
        Some("full") => return "Full Reset".to_owned(),
        _ => {}
    }
    let session = clears.contains(&"five_hour");
    let other = clears.iter().any(|window| *window != "five_hour");
    if session && !other {
        "Session Reset".to_owned()
    } else if other && !session {
        "Weekly Reset".to_owned()
    } else {
        "Full Reset".to_owned()
    }
}

fn grant_detail(clears: &[&str], gives_back_weekly: bool) -> Option<String> {
    let mut names = Vec::with_capacity(clears.len());
    for window in clears {
        match *window {
            "five_hour" => names.push("session"),
            "seven_day" => names.push("weekly"),
            "seven_day_overage_included" => names.push("Fable"),
            _ => return None,
        }
    }
    let last = names.pop()?;
    let mut detail = if names.is_empty() {
        format!("Refills your {last} limit now")
    } else {
        format!("Refills your {} and {last} limits now", names.join(", "))
    };
    if gives_back_weekly && !clears.contains(&"seven_day") {
        detail.push_str(" and gives back what Fable used of your weekly limit");
    }
    detail.push_str(". This cannot be undone.");
    Some(detail)
}

fn parse_redeem_response(
    status: u16,
    retry_after: Option<Duration>,
    raw_body: &str,
    now: Timestamp,
) -> Result<RedeemOutcome, RedeemError> {
    if status == 403 {
        let message = if raw_body.contains("scope requirement") {
            "This Claude login does not have permission to use resets. Sign in with Claude again, then re-add the account."
        } else {
            "Claude refused the limit reset. This login may not have permission to use resets."
        };
        return Err(RedeemError::Refused(message.to_owned()));
    }
    if status == 401 {
        return Err(RedeemError::Remote(RemoteError::Unauthorized));
    }
    if status == 429 {
        return Err(RedeemError::Remote(RemoteError::RateLimited(retry_after)));
    }
    if status >= 500 {
        return Err(RedeemError::Unsettled(
            "Claude returned a server error while applying the reset. Try again.".to_owned(),
        ));
    }
    if !(200..300).contains(&status) {
        return Err(RedeemError::Refused("Claude refused the limit reset. Please try again.".to_owned()));
    }
    let unsettled = || {
        RedeemError::Unsettled(
            "Could not confirm whether the reset was applied. Refresh in a moment to check.".to_owned(),
        )
    };
    let body: Value = serde_json::from_str(raw_body).map_err(|_| unsettled())?;
    let result = body.get("result").and_then(Value::as_str).ok_or_else(unsettled)?;
    let code = if result == "unavailable" {
        body.get("reason")
            .and_then(Value::as_str)
            .ok_or_else(|| RedeemError::Refused("Claude refused the limit reset. Please try again.".to_owned()))?
    } else {
        result
    };
    match code {
        "reset" | "already_used" => {
            let remaining = body
                .get("resets_left")
                .and_then(Value::as_f64)
                .filter(|left| left.is_finite())
                .map(|left| left.floor().max(0.0) as u32);
            Ok(RedeemOutcome { remaining })
        }
        "not_limited" => Err(RedeemError::Refused("There is no usage to reset right now.".to_owned())),
        "ineligible" | "no_grant" | "unknown_grant" => {
            Err(RedeemError::Exhausted("That limit reset is no longer available.".to_owned()))
        }
        "not_next_grant" => Err(RedeemError::NotNextGrant),
        "reset_unconfirmed" | "stamp_indeterminate" => Err(unsettled()),
        "cooldown" | "paused" | "low_priority_active" => {
            let until = body.get("cooldown_until").and_then(flexible_timestamp).filter(|at| *at > now);
            let message = match until {
                Some(at) => format!(
                    "Resets cannot be used right now. Try again in {}.",
                    format_wait(at.duration_since(now).as_secs())
                ),
                None => "Resets cannot be used right now.".to_owned(),
            };
            Err(RedeemError::Refused(message))
        }
        _ => Err(RedeemError::Refused("Claude refused the limit reset. Please try again.".to_owned())),
    }
}

fn format_wait(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => "<1m".to_owned(),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_reads_every_window() {
        let windows = parse_usage(&json!({
            "five_hour": {"utilization": 97.0, "resets_at": "2026-11-24T23:29:59.123456+00:00"},
            "seven_day": {"utilization": 16, "resets_at": null},
            "limits": [{"percent": 40, "scope": {"model": {"display_name": "Fable"}}}, {"percent": 1, "scope": null}]
        }))
        .unwrap();
        let summary: Vec<_> = windows.iter().map(|w| (w.label.as_str(), w.used_percent, w.primary)).collect();
        assert_eq!(
            summary,
            [("Session", Some(97.0), true), ("Weekly", Some(16.0), true), ("Fable", Some(40.0), false)]
        );
        assert!(windows[0].resets_at.is_some());
    }

    #[test]
    fn a_missing_window_is_left_out_and_a_null_measurement_is_kept() {
        let windows = parse_usage(&json!({"five_hour": null, "seven_day": {"utilization": null}})).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].used_percent, None);
    }

    #[test]
    fn malformed_usage_is_an_error() {
        assert!(parse_usage(&json!({"five_hour": {"utilization": "97"}})).is_err());
        assert!(
            parse_usage(&json!({"five_hour": {"resets_at": null, "x": 1}, "seven_day": {"utilization": -1}})).is_err()
        );
        assert!(parse_usage(&json!({})).is_err());
    }

    #[test]
    fn profile_needs_an_account_id() {
        assert!(parse_profile(&json!({"account": {"email": "a@example.com"}})).is_err());
        let profile = parse_profile(&json!({
            "account": {"uuid": "a", "email": "a@example.com"},
            "organization": {"uuid": "o", "name": "Org", "organization_type": "claude_max",
                "rate_limit_tier": "default_claude_max_20x", "plan_display_name": "Max"}
        }))
        .unwrap();
        assert_eq!(profile.plan.as_deref(), Some("Max 20x"));
    }

    #[test]
    fn refresh_keeps_the_old_token_when_none_is_sent() {
        let now = Timestamp::from_second(1000).unwrap();
        let tokens = parse_tokens(&json!({"access_token": "x", "expires_in": 10}), now).unwrap();
        assert_eq!(tokens.expires_at_ms, 1_010_000);
        assert!(tokens.refresh.is_none());
    }

    #[test]
    fn reset_grants_handles_absent_ineligible_and_all_reset_kinds() {
        assert_eq!(parse_reset_grants(&json!({})), ResetGrants::default());
        assert_eq!(parse_reset_grants(&json!({"cedar_ember": null})), ResetGrants::default());
        assert_eq!(
            parse_reset_grants(&json!({
                "cedar_ember": {
                    "eligible": false,
                    "grants": [{"id": "g1", "resets_left": 1, "usable_now": true}]
                }
            })),
            ResetGrants::default()
        );

        let parsed = parse_reset_grants(&json!({
            "cedar_ember": {
                "eligible": true,
                "next_grant_id": "g_full",
                "grants": [
                    {
                        "id": "g_full",
                        "resets_left": 1,
                        "ends_at": "2026-12-10T17:00:00+00:00",
                        "clears": ["five_hour", "seven_day", "seven_day_overage_included"],
                        "usable_now": true,
                        "paused": false
                    },
                    {
                        "id": "g_fable",
                        "resets_left": 2.0,
                        "ends_at": 1_800_000_000,
                        "type": "weekly",
                        "gives_back_weekly": true,
                        "clears": ["five_hour", "seven_day_overage_included"],
                        "usable_now": true,
                        "paused": true
                    },
                    {
                        "id": "g_session",
                        "resets_left": 1,
                        "clears": ["five_hour"],
                        "usable_now": true
                    },
                    {
                        "id": "g_weekly",
                        "resets_left": 1,
                        "clears": ["seven_day"],
                        "usable_now": false
                    },
                    {
                        "id": "g_empty",
                        "resets_left": 0,
                        "usable_now": true
                    }
                ]
            }
        }));

        assert_eq!(parsed.next_grant_id.as_deref(), Some("g_full"));
        assert_eq!(parsed.resets.len(), 4);
        assert_eq!(parsed.resets[0].label, "Full Reset");
        assert!(parsed.resets[0].usable);
        assert_eq!(
            parsed.resets[0].detail.as_deref(),
            Some("Refills your session, weekly and Fable limits now. This cannot be undone.")
        );
        assert_eq!(parsed.resets[1].label, "Fable Reset");
        assert!(!parsed.resets[1].usable);
        assert_eq!(
            parsed.resets[1].detail.as_deref(),
            Some(
                "Refills your session and Fable limits now and gives back what Fable used of your weekly limit. This cannot be undone."
            )
        );
        assert_eq!(parsed.resets[2].label, "Session Reset");
        assert_eq!(parsed.resets[3].label, "Weekly Reset");
        assert!(!parsed.resets[3].usable);
    }

    #[test]
    fn malformed_reset_grants_are_skipped() {
        let parsed = parse_reset_grants(&json!({
            "cedar_ember": {
                "eligible": true,
                "grants": [
                    "not_an_object",
                    {"id": ""},
                    {"id": "g_missing_left"},
                    {"id": "g_bad_left", "resets_left": "1"},
                    {"id": "g_ok", "resets_left": 1, "clears": ["five_hour"], "usable_now": true}
                ]
            }
        }));
        assert_eq!(parsed.resets.len(), 1);
        assert_eq!(parsed.resets[0].id, "g_ok");

        assert_eq!(
            parse_reset_grants(&json!({
                "cedar_ember": {
                    "eligible": false,
                    "grants": [{"id": ""}]
                }
            })),
            ResetGrants::default()
        );
    }

    #[test]
    fn redeem_responses_map_every_outcome() {
        let now = Timestamp::from_second(1_000_000).unwrap();
        let ok = parse_redeem_response(200, None, r#"{"result":"reset","resets_left":0}"#, now).unwrap();
        assert_eq!(ok.remaining, Some(0));
        let already = parse_redeem_response(200, None, r#"{"result":"already_used"}"#, now).unwrap();
        assert_eq!(already.remaining, None);

        assert!(matches!(
            parse_redeem_response(200, None, r#"{"result":"not_limited"}"#, now),
            Err(RedeemError::Refused(_))
        ));
        assert!(matches!(
            parse_redeem_response(200, None, r#"{"result":"unavailable","reason":"no_grant"}"#, now),
            Err(RedeemError::Exhausted(_))
        ));
        assert!(matches!(
            parse_redeem_response(200, None, r#"{"result":"not_next_grant"}"#, now),
            Err(RedeemError::NotNextGrant)
        ));
        assert!(matches!(
            parse_redeem_response(200, None, r#"{"result":"reset_unconfirmed"}"#, now),
            Err(RedeemError::Unsettled(_))
        ));
        match parse_redeem_response(
            200,
            None,
            r#"{"result":"unavailable","reason":"cooldown","cooldown_until":1005400}"#,
            now,
        ) {
            Err(RedeemError::Refused(msg)) => assert_eq!(msg, "Resets cannot be used right now. Try again in 1h 30m."),
            other => panic!("unexpected result: {other:?}"),
        }
        assert!(matches!(
            parse_redeem_response(
                403,
                None,
                r#"{"error":{"message":"OAuth token does not meet scope requirement"}}"#,
                now
            ),
            Err(RedeemError::Refused(_))
        ));
        assert!(matches!(
            parse_redeem_response(403, None, r#"{"error":{"message":"forbidden"}}"#, now),
            Err(RedeemError::Refused(_))
        ));
        assert!(matches!(
            parse_redeem_response(401, None, "{}", now),
            Err(RedeemError::Remote(RemoteError::Unauthorized))
        ));
        assert!(matches!(
            parse_redeem_response(429, Some(Duration::from_secs(60)), "{}", now),
            Err(RedeemError::Remote(RemoteError::RateLimited(Some(_))))
        ));
        assert!(matches!(parse_redeem_response(503, None, "", now), Err(RedeemError::Unsettled(_))));
        assert!(!valid_grant_id("Bad/Grant"));
        assert!(valid_grant_id("opus55-launch_1"));
        assert!(valid_org_id("org-123"));
        assert!(!valid_org_id("../other"));
        assert!(valid_request_id("req-123_ABC"));
    }
}
