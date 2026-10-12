//! Session secrets: random tokens and keys, and how a guess is compared
//! with them.

use std::io::Read;

use crate::{NdError, Result};

/// Fills `buf` from the kernel's random source.
pub fn random_bytes(buf: &mut [u8]) -> Result<()> {
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|e| NdError::Network(format!("could not generate a session secret: {e}")))
}

/// A random 128-bit token in lowercase hexadecimal.
pub fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    random_bytes(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compares a guess with a secret without stopping at the first difference,
/// so how long a wrong guess takes does not say how much of it was right.
/// The length is not secret: tokens and PINs have fixed sizes.
pub fn same_secret(guess: &str, secret: &str) -> bool {
    let difference = guess
        .bytes()
        .zip(secret.bytes())
        .fold(0, |difference, (a, b)| difference | (a ^ b));
    guess.len() == secret.len() && std::hint::black_box(difference) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_128_bits_of_hex() {
        let token = random_token().expect("/dev/urandom");
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(token, random_token().unwrap());
    }

    #[test]
    fn secrets_match_only_in_full() {
        assert!(same_secret("feedface", "feedface"));
        for guess in ["feedfacf", "feedfac", "feedface0", "", "Feedface"] {
            assert!(!same_secret(guess, "feedface"), "{guess}");
        }
    }
}
