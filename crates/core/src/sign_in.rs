use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::{SignedDuration, Timestamp};
use sha2::{Digest, Sha256};

const LIFETIME: SignedDuration = SignedDuration::from_mins(10);
const MAX_PENDING: usize = 16;

/// A browser sign-in waiting for its code. The verifier proves the code was asked for here.
pub(crate) struct Pending {
    pub provider: String,
    pub state: String,
    pub verifier: String,
    pub redirect_uri: String,
    started: Timestamp,
}

#[derive(Default)]
pub(crate) struct SignIns {
    pending: Vec<Pending>,
}

impl SignIns {
    /// Returns the state and the PKCE code challenge for a new sign-in.
    pub fn begin(&mut self, provider: &str, redirect_uri: &str, now: Timestamp) -> (String, String) {
        self.forget_expired(now);
        if self.pending.len() >= MAX_PENDING {
            self.pending.remove(0);
        }
        let state = random();
        let verifier = format!("{}{}", random(), random());
        let challenge = code_challenge(&verifier);
        self.pending.push(Pending {
            provider: provider.to_owned(),
            state: state.clone(),
            verifier,
            redirect_uri: redirect_uri.to_owned(),
            started: now,
        });
        (state, challenge)
    }

    /// Each sign-in can be finished once, and only before it expires.
    pub fn take(&mut self, provider: &str, state: &str, now: Timestamp) -> Option<Pending> {
        self.forget_expired(now);
        let index = self.pending.iter().position(|pending| pending.provider == provider && pending.state == state)?;
        Some(self.pending.remove(index))
    }

    fn forget_expired(&mut self, now: Timestamp) {
        self.pending.retain(|pending| now < pending.started + LIFETIME);
    }
}

fn random() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(second: i64) -> Timestamp {
        Timestamp::from_second(second).unwrap()
    }

    #[test]
    fn the_challenge_follows_rfc_7636() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_sign_in_finishes_once_with_its_own_state_and_provider() {
        let mut sign_ins = SignIns::default();
        let (state, challenge) = sign_ins.begin("antigravity", "http://127.0.0.1:1/cb", at(0));
        assert!(sign_ins.take("antigravity", "wrong", at(1)).is_none());
        assert!(sign_ins.take("claude", &state, at(1)).is_none());
        let pending = sign_ins.take("antigravity", &state, at(1)).unwrap();
        assert_eq!(code_challenge(&pending.verifier), challenge);
        assert_eq!(pending.redirect_uri, "http://127.0.0.1:1/cb");
        assert!(sign_ins.take("antigravity", &state, at(2)).is_none());
    }

    #[test]
    fn an_old_sign_in_expires() {
        let mut sign_ins = SignIns::default();
        let (state, _) = sign_ins.begin("antigravity", "http://127.0.0.1:1/cb", at(0));
        assert!(sign_ins.take("antigravity", &state, at(600)).is_none());
    }

    #[test]
    fn only_the_latest_sign_ins_are_kept() {
        let mut sign_ins = SignIns::default();
        let states: Vec<String> =
            (0..=MAX_PENDING).map(|_| sign_ins.begin("antigravity", "http://127.0.0.1:1/cb", at(0)).0).collect();
        assert!(sign_ins.take("antigravity", &states[0], at(1)).is_none());
        assert!(sign_ins.take("antigravity", &states[MAX_PENDING], at(1)).is_some());
    }
}
