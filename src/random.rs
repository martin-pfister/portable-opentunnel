//! Random tokens, read from the OS.
//!
//! `/dev/urandom` is the only source: routes and directory names must not be
//! guessable, and no extra dependency is needed for a few dozen bytes.

use std::io::Read;

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Fills `buffer` from the kernel CSPRNG, panicking only if the OS cannot
/// provide randomness at all (in which case no secure name can be chosen).
fn fill(buffer: &mut [u8]) {
    let mut file =
        std::fs::File::open("/dev/urandom").expect("the kernel random device must be available");
    file.read_exact(buffer)
        .expect("reading from the kernel random device");
}

fn token(length: usize) -> String {
    let mut bytes = vec![0u8; length];
    fill(&mut bytes);
    bytes
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect()
}

/// A random lowercase-alphanumeric string.
pub fn alphanumeric(length: usize) -> String {
    token(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_the_requested_shape() {
        let value = alphanumeric(12);
        assert_eq!(value.len(), 12);
        assert!(
            value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
    }

    #[test]
    fn tokens_differ() {
        let values: std::collections::HashSet<String> = (0..64).map(|_| alphanumeric(16)).collect();
        assert_eq!(values.len(), 64, "tokens must not repeat");
    }
}
