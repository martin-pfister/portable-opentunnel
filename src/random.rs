//! Random names, read from the OS.
//!
//! `/dev/urandom` is the only source: routes and directory names must not be
//! guessable, and no extra dependency is needed for a few dozen bytes.

use std::io::Read;

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Bytes at or above this are rejected, so every symbol is equally likely.
const UNBIASED_LIMIT: u8 = (256 / ALPHABET.len() * ALPHABET.len()) as u8;

/// A random lowercase-alphanumeric string.
///
/// Panics only if the kernel cannot provide randomness at all, in which case no
/// unguessable name can be chosen.
pub fn alphanumeric(length: usize) -> String {
    let mut source =
        std::fs::File::open("/dev/urandom").expect("the kernel random device must be available");
    let mut value = String::with_capacity(length);
    let mut buffer = [0u8; 64];
    while value.len() < length {
        source
            .read_exact(&mut buffer)
            .expect("reading from the kernel random device");
        value.extend(
            buffer
                .iter()
                .filter(|&&byte| byte < UNBIASED_LIMIT)
                .map(|&byte| ALPHABET[usize::from(byte) % ALPHABET.len()] as char)
                .take(length - value.len()),
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_have_the_requested_shape() {
        for length in [0, 1, 12, 100] {
            let value = alphanumeric(length);
            assert_eq!(value.len(), length);
            assert!(
                value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            );
        }
    }

    #[test]
    fn values_differ() {
        let values: std::collections::HashSet<String> = (0..64).map(|_| alphanumeric(16)).collect();
        assert_eq!(values.len(), 64, "values must not repeat");
    }
}
