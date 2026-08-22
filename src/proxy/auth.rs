//! The per-instance proxy credential, and the two encodings it arrives in.
//!
//! The proxy listens on loopback, which every process on the host can reach —
//! including the ones a sandbox exists to contain. The token is what makes the
//! listener belong to *one* sandbox: it is generated per instance, handed to
//! the sandbox through its proxy environment variables, and never written to a
//! log or an error body.

/// The username the proxy publishes in its `http://user:token@host:port` URL.
///
/// HTTP `Basic` and SOCKS5 both carry a username beside the secret and neither
/// has a way to omit it; the value is ignored on the way back in, so it only
/// has to be stable and recognisable in a client's configuration.
pub const PROXY_USERNAME: &str = "friring";

/// Mint a fresh token: 128 bits from the OS CSPRNG, hex-encoded.
///
/// Reuses the UUID v4 generator the crate already depends on rather than
/// adding a random-number dependency; v4 is defined to draw its 122 free bits
/// from a cryptographically secure source.
pub fn generate_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Compare a presented secret against the expected one without branching on
/// content.
///
/// Length is compared first and therefore leaks, which is deliberate: the
/// token's length is a constant of the build, not a secret, and a
/// variable-length loop would leak far more.
pub fn secret_eq(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in presented.iter().zip(expected) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Decode standard base64 (RFC 4648, `+/` alphabet, optional `=` padding).
///
/// Only exists to read a `Proxy-Authorization: Basic` header, so it is strict:
/// any character outside the alphabet — including the whitespace some encoders
/// wrap at column 76 — rejects the whole input rather than being skipped, and
/// a malformed credential is refused like a wrong one.
pub fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(encoded.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    let mut padding = 0usize;
    for byte in encoded.bytes() {
        if byte == b'=' {
            padding += 1;
            continue;
        }
        // Padding is terminal: `A=A=` is not a valid encoding of anything.
        if padding > 0 {
            return None;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
            // Drop the bits just emitted, so the accumulator stays under six
            // bits and the shift above cannot overflow on a long input.
            accumulator &= (1u32 << bits) - 1;
        }
    }
    // A well-formed group is four characters, and the bits left over from a
    // shorter one are defined to be zero.
    if padding > 2 || bits >= 6 || accumulator != 0 {
        return None;
    }
    Some(out)
}

/// Whether a `Proxy-Authorization` header value presents `token`.
///
/// Both encodings a real client produces are accepted: `Bearer <token>`, which
/// is what Friring's own documentation shows, and `Basic
/// base64(user:token)`, which is what every HTTP client derives on its own
/// from a `http://user:token@host:port` proxy URL. The username is not checked
/// — the token is the credential.
pub fn header_presents_token(header: &str, token: &str) -> bool {
    let Some((scheme, credential)) = header.split_once(' ') else {
        return false;
    };
    let credential = credential.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        return secret_eq(credential, token);
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let Some(decoded) = decode_base64(credential) else {
            return false;
        };
        let Ok(pair) = String::from_utf8(decoded) else {
            return false;
        };
        // `user:pass` splits on the *first* colon; a token never contains one,
        // but a username might.
        return match pair.split_once(':') {
            Some((_, password)) => secret_eq(password, token),
            None => false,
        };
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal encoder so the tests state their fixtures as plain text.
    fn encode_base64(input: &str) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = input.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut group = [0u8; 3];
            group[..chunk.len()].copy_from_slice(chunk);
            let triple = u32::from_be_bytes([0, group[0], group[1], group[2]]);
            for shift in [18, 12, 6, 0] {
                out.push(ALPHABET[((triple >> shift) & 0x3f) as usize] as char);
            }
            let dropped = 3 - chunk.len();
            out.truncate(out.len() - dropped);
            out.extend(std::iter::repeat('=').take(dropped));
        }
        out
    }

    #[test]
    fn tokens_are_unique_and_long_enough_to_be_unguessable() {
        let (first, second) = (generate_token(), generate_token());
        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn secret_comparison_accepts_only_the_exact_value() {
        assert!(secret_eq("s3cret", "s3cret"));
        assert!(!secret_eq("s3cret", "s3crets"));
        assert!(!secret_eq("s3crets", "s3cret"));
        assert!(!secret_eq("", "s3cret"));
        assert!(!secret_eq("S3CRET", "s3cret"));
    }

    #[test]
    fn base64_round_trips_every_padding_length() {
        for text in ["", "a", "ab", "abc", "abcd", "friring:0123456789abcdef"] {
            let encoded = encode_base64(text);
            let decoded = decode_base64(&encoded).expect("round-trips");
            assert_eq!(decoded, text.as_bytes(), "round-trip of `{text}`");
        }
    }

    #[test]
    fn malformed_base64_is_rejected_rather_than_repaired() {
        for encoded in ["a b", "****", "AB=A", "A===", "ab-c", "AB\n"] {
            assert!(
                decode_base64(encoded).is_none(),
                "`{encoded}` must not decode"
            );
        }
    }

    #[test]
    fn both_credential_encodings_are_accepted() {
        let token = "0123456789abcdef";
        assert!(header_presents_token(&format!("Bearer {token}"), token));
        assert!(header_presents_token(&format!("bearer {token}"), token));
        let basic = encode_base64(&format!("{PROXY_USERNAME}:{token}"));
        assert!(header_presents_token(&format!("Basic {basic}"), token));
        assert!(header_presents_token(&format!("BASIC {basic}"), token));
        // The username is not the credential.
        let other_user = encode_base64(&format!("someone-else:{token}"));
        assert!(header_presents_token(&format!("Basic {other_user}"), token));
    }

    #[test]
    fn wrong_or_malformed_credentials_are_refused() {
        let token = "0123456789abcdef";
        let cases = [
            String::from("Bearer wrong"),
            String::from("Bearer"),
            String::new(),
            format!(
                "Basic {}",
                encode_base64(&format!("{PROXY_USERNAME}:wrong"))
            ),
            // A token with no `user:` prefix is not a Basic credential.
            format!("Basic {}", encode_base64(token)),
            format!("Digest {token}"),
            format!("Basic {token}"),
        ];
        for header in cases {
            assert!(
                !header_presents_token(&header, token),
                "accepted `{header}`"
            );
        }
    }
}
