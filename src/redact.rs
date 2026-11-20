//! Secret redaction for diagnostics.
//!
//! Some SDK errors carry text this program did not write, such as a bridge
//! upgrade response. A misbehaving endpoint can echo the request, and with it
//! the saved token, straight back. Such text passes through
//! [`Secrets::scrub`] before it reaches stderr.

/// The credential values an invocation knows about.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    values: Vec<String>,
}

impl Secrets {
    /// Shorter values are not redacted: replacing them would mangle unrelated
    /// text without hiding anything.
    const MIN_LEN: usize = 8;

    /// Every credential a complete identity carries.
    pub fn from_identity(identity: &opentunnel::Identity) -> Self {
        let mut secrets = Self::default();
        for value in [
            &identity.token,
            &identity.private_key,
            &identity.certificate,
            &identity.chain,
        ] {
            secrets.add(value);
        }
        secrets
    }

    fn add(&mut self, value: &str) {
        if value.len() >= Self::MIN_LEN && !self.values.iter().any(|known| known == value) {
            self.values.push(value.to_owned());
        }
    }

    /// Replaces each known secret with `<redacted>`.
    pub fn scrub(&self, text: &str) -> String {
        self.values.iter().fold(text.to_owned(), |text, value| {
            text.replace(value.as_str(), "<redacted>")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(values: &[&str]) -> Secrets {
        let mut secrets = Secrets::default();
        for value in values {
            secrets.add(value);
        }
        secrets
    }

    #[test]
    fn replaces_known_values() {
        assert_eq!(
            secrets(&["token-abcdef123"]).scrub("rejected token-abcdef123 here"),
            "rejected <redacted> here"
        );
    }

    #[test]
    fn ignores_values_too_short_to_be_meaningful() {
        assert_eq!(secrets(&["short"]).scrub("a short word"), "a short word");
    }
}
