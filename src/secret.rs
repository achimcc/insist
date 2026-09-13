use std::fmt;

/// A string that must never reach a log line, an error message or a Debug
/// dump. Tokens, the HMAC key, topic names and the watchdog URL are the
/// values this project cannot afford to leak; this type is the structural
/// answer to that, not a policy anyone has to remember.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether an offered value equals this secret, in time that does not
    /// depend on how many leading bytes match. A plain `==` returns early at
    /// the first differing byte and leaks the prefix to anyone who can time
    /// the answer — relevant once a webhook is authenticated with this.
    pub fn matches(&self, offered: &str) -> bool {
        let secret = self.0.as_bytes();
        let offered = offered.as_bytes();
        // The length is not itself a secret, and comparing unequal-length
        // slices below would need a branch anyway.
        if secret.len() != offered.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in secret.iter().zip(offered) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Secret(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_value() {
        let s = Secret::from("hunter2-and-then-some".to_string());
        let shown = format!("{s:?}");
        assert!(!shown.contains("hunter2"), "leaked: {shown}");
        assert_eq!(shown, "<redacted>");
    }

    #[test]
    fn matches_accepts_the_exact_value_and_nothing_else() {
        let s = Secret::from("t-o-k-e-n".to_string());
        assert!(s.matches("t-o-k-e-n"));
        assert!(!s.matches("t-o-k-e-m"), "one byte off must not pass");
        assert!(!s.matches("t-o-k-e"), "a prefix must not pass");
        assert!(!s.matches("t-o-k-e-n-x"), "an extension must not pass");
        assert!(!s.matches(""), "empty must not pass");
    }
}
