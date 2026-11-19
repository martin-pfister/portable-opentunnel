//! Secret redaction for diagnostics.
//!
//! Some SDK errors carry text this program did not write: the API's own error
//! body, or a bridge upgrade response. A misbehaving (or test) endpoint can
//! echo the request — and therefore the saved token — straight back. Nothing
//! that reaches stderr may contain a credential, so the few places that render
//! such text pass it through [`Secrets::scrub`] first.

/// The credential values an invocation knows about.
///
/// Only values long enough to be meaningful are kept: replacing a one-character
/// string would mangle unrelated text without hiding anything.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    values: Vec<String>,
}

impl Secrets {
    /// The shortest value that is still treated as a secret.
    const MIN_LEN: usize = 8;

    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a value to remove from later diagnostics.
    pub fn add(&mut self, value: impl Into<String>) {
        let value = value.into();
        if value.len() >= Self::MIN_LEN && !self.values.contains(&value) {
            self.values.push(value);
        }
    }

    /// Registers every credential a complete identity carries.
    pub fn from_identity(identity: &opentunnel::Identity) -> Self {
        let mut secrets = Self::new();
        secrets.add_identity(identity);
        secrets
    }

    /// Registers every credential a complete identity carries.
    pub fn add_identity(&mut self, identity: &opentunnel::Identity) {
        self.add(identity.token.clone());
        self.add(identity.private_key.clone());
        self.add(identity.certificate.clone());
        self.add(identity.chain.clone());
    }

    /// Replaces each known secret with `<redacted>`.
    pub fn scrub(&self, text: &str) -> String {
        let mut scrubbed = text.to_owned();
        for value in &self.values {
            if scrubbed.contains(value.as_str()) {
                scrubbed = scrubbed.replace(value.as_str(), "<redacted>");
            }
        }
        scrubbed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_known_values() {
        let mut secrets = Secrets::new();
        secrets.add("token-abcdef123");
        assert_eq!(
            secrets.scrub("rejected token-abcdef123 here"),
            "rejected <redacted> here"
        );
    }

    #[test]
    fn ignores_values_too_short_to_be_meaningful() {
        let mut secrets = Secrets::new();
        secrets.add("short");
        assert_eq!(secrets.scrub("a short word"), "a short word");
    }

    #[test]
    fn leaves_unrelated_text_untouched() {
        let mut secrets = Secrets::new();
        secrets.add("token-abcdef123");
        assert_eq!(secrets.scrub("HTTP status 401"), "HTTP status 401");
    }
}
