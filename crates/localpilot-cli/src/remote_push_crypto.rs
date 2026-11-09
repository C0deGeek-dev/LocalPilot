//! Web Push message encryption (RFC 8291 over RFC 8188 `aes128gcm`) and the
//! VAPID signature (RFC 8292), composed on `ring`.
//!
//! Pure functions: no files, no network. The sender gives a subscription's
//! public key and authentication secret, a short plaintext and a fixed padded
//! size; the result is the request body. The VAPID part signs a token for one
//! push-service origin with the sender's key.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::rand::SecureRandom as _;
use ring::{aead, agreement, hkdf, rand, signature};

/// Uncompressed P-256 public key length (0x04 || X || Y).
pub(crate) const PUBLIC_KEY_LEN: usize = 65;
/// Authentication secret length the browser hands out.
pub(crate) const AUTH_LEN: usize = 16;
const SALT_LEN: usize = 16;
const TAG_LEN: usize = 16;
/// The single record's size, written in the header. One record always holds
/// the whole padded message, and must be strictly smaller (RFC 8291 section 4).
const RECORD_SIZE: u32 = 4096;
/// The largest request body every push service accepts (RFC 8030 section
/// 7.2), header included.
const MAX_BODY: usize = 4096;
/// The header: salt, record size, key-id length, the sender's public key.
const HEADER_LEN: usize = SALT_LEN + 4 + 1 + PUBLIC_KEY_LEN;

/// Why a message could not be encrypted or signed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PushCryptoError {
    #[error("the subscription's public key is not a P-256 point")]
    BadPublicKey,
    #[error("the subscription's authentication secret has the wrong length")]
    BadAuth,
    #[error("the message does not fit its padded size")]
    TooLong,
    #[error("the system random source failed")]
    Random,
    #[error("the encryption step failed")]
    Seal,
    #[error("the push key could not sign")]
    Sign,
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[u8], out: &mut [u8]) -> Result<(), PushCryptoError> {
    prk.expand(&[info], Len(out.len()))
        .and_then(|okm| okm.fill(out))
        .map_err(|_| PushCryptoError::Seal)
}

/// The content key and nonce for one message (RFC 8291 section 3.4, then
/// RFC 8188 section 2.2).
fn derive(
    ecdh_secret: &[u8],
    auth: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> Result<([u8; 16], [u8; 12]), PushCryptoError> {
    let mut key_info = Vec::with_capacity(14 + 2 * PUBLIC_KEY_LEN);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(ecdh_secret);
    let mut ikm = [0u8; 32];
    expand(&prk_key, &key_info, &mut ikm)?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let mut cek = [0u8; 16];
    expand(&prk, b"Content-Encoding: aes128gcm\0", &mut cek)?;
    let mut nonce = [0u8; 12];
    expand(&prk, b"Content-Encoding: nonce\0", &mut nonce)?;
    Ok((cek, nonce))
}

/// Encrypt `plaintext` as one `aes128gcm` record padded to `padded` bytes
/// (before the tag), from an already agreed secret. The body is the header
/// (salt, record size, the sender's public key) followed by the record.
fn seal_with(
    ecdh_secret: &[u8],
    auth: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8; SALT_LEN],
    plaintext: &[u8],
    padded: usize,
) -> Result<Vec<u8>, PushCryptoError> {
    // The delimiter octet (0x02, last record) follows the plaintext; zeros
    // fill the rest. The record (padded + tag) stays below the record size,
    // and the whole body within what every push service accepts.
    let fits = plaintext.len() < padded
        && padded
            .checked_add(TAG_LEN + HEADER_LEN)
            .is_some_and(|body| body <= MAX_BODY && body - HEADER_LEN < RECORD_SIZE as usize);
    if !fits || as_public.len() != PUBLIC_KEY_LEN {
        return Err(PushCryptoError::TooLong);
    }
    let (cek, nonce) = derive(ecdh_secret, auth, ua_public, as_public, salt)?;
    let mut record = Vec::with_capacity(padded + TAG_LEN);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    record.resize(padded, 0);
    let key = aead::UnboundKey::new(&aead::AES_128_GCM, &cek)
        .map(aead::LessSafeKey::new)
        .map_err(|_| PushCryptoError::Seal)?;
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::empty(),
        &mut record,
    )
    .map_err(|_| PushCryptoError::Seal)?;
    let mut body = Vec::with_capacity(SALT_LEN + 5 + as_public.len() + record.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    // Both lengths are fixed by the curve; the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    body.push(as_public.len() as u8);
    body.extend_from_slice(as_public);
    body.extend_from_slice(&record);
    Ok(body)
}

/// Encrypt `plaintext` for one subscription: a fresh sender key pair and salt
/// for every message, padded to `padded` bytes so every message of a kind has
/// the same size.
pub(crate) fn encrypt(
    ua_public: &[u8],
    auth: &[u8],
    plaintext: &[u8],
    padded: usize,
) -> Result<Vec<u8>, PushCryptoError> {
    if ua_public.len() != PUBLIC_KEY_LEN || ua_public.first() != Some(&0x04) {
        return Err(PushCryptoError::BadPublicKey);
    }
    if auth.len() != AUTH_LEN {
        return Err(PushCryptoError::BadAuth);
    }
    let rng = rand::SystemRandom::new();
    let mut salt = [0u8; SALT_LEN];
    rng.fill(&mut salt).map_err(|_| PushCryptoError::Random)?;
    let sender = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| PushCryptoError::Random)?;
    let as_public = sender
        .compute_public_key()
        .map_err(|_| PushCryptoError::Random)?;
    let as_public = as_public.as_ref().to_vec();
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let ecdh_secret = agreement::agree_ephemeral(sender, &peer, <[u8]>::to_vec)
        .map_err(|_| PushCryptoError::BadPublicKey)?;
    seal_with(
        &ecdh_secret,
        auth,
        ua_public,
        &as_public,
        &salt,
        plaintext,
        padded,
    )
}

/// The `Authorization` header value for one push-service origin: a VAPID
/// token (ES256, `aud` and `exp` only) and the sender's public key.
pub(crate) fn vapid_authorization(
    key: &signature::EcdsaKeyPair,
    audience: &str,
    expires_unix: u64,
) -> Result<String, PushCryptoError> {
    use signature::KeyPair as _;
    let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = serde_json::json!({ "aud": audience, "exp": expires_unix });
    let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
    let signing_input = format!("{header}.{claims}");
    let rng = rand::SystemRandom::new();
    let signed = key
        .sign(&rng, signing_input.as_bytes())
        .map_err(|_| PushCryptoError::Sign)?;
    let token = format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signed.as_ref())
    );
    let public = URL_SAFE_NO_PAD.encode(key.public_key().as_ref());
    Ok(format!("vapid t={token}, k={public}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use signature::KeyPair as _;

    fn b64(text: &str) -> Vec<u8> {
        let compact: String = text.split_whitespace().collect();
        URL_SAFE_NO_PAD.decode(compact).unwrap()
    }

    // RFC 8291 section 5 and Appendix A.
    const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const UA_PUBLIC: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx
        aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AS_PUBLIC: &str = "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIg
        Dll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const ECDH_SECRET: &str = "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs";
    const PLAINTEXT: &str = "V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24";
    const BODY: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml
        mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT
        pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    #[test]
    fn the_rfc_8291_example_encrypts_to_its_exact_body() {
        let (cek, nonce) = derive(
            &b64(ECDH_SECRET),
            &b64(AUTH),
            &b64(UA_PUBLIC),
            &b64(AS_PUBLIC),
            &b64(SALT),
        )
        .unwrap();
        assert_eq!(cek.to_vec(), b64("oIhVW04MRdy2XN9CiKLxTg"));
        assert_eq!(nonce.to_vec(), b64("4h_95klXJ5E_qnoN"));
        let plaintext = b64(PLAINTEXT);
        let salt: [u8; 16] = b64(SALT).try_into().unwrap();
        let body = seal_with(
            &b64(ECDH_SECRET),
            &b64(AUTH),
            &b64(UA_PUBLIC),
            &b64(AS_PUBLIC),
            &salt,
            &plaintext,
            plaintext.len() + 1,
        )
        .unwrap();
        assert_eq!(body, b64(BODY));
    }

    /// The receiver's side, for the round trip: agree from the header's key,
    /// derive, open, strip padding.
    fn open(
        receiver: agreement::EphemeralPrivateKey,
        ua_public: &[u8],
        auth: &[u8],
        body: &[u8],
    ) -> Vec<u8> {
        let salt = &body[..16];
        assert_eq!(&body[16..20], &RECORD_SIZE.to_be_bytes());
        let id_len = usize::from(body[20]);
        let as_public = &body[21..21 + id_len];
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
        let ecdh = agreement::agree_ephemeral(receiver, &peer, <[u8]>::to_vec).unwrap();
        let (cek, nonce) = derive(&ecdh, auth, ua_public, as_public, salt).unwrap();
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
        let mut record = body[21 + id_len..].to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::empty(),
                &mut record,
            )
            .unwrap()
            .to_vec();
        let end = plain.iter().rposition(|byte| *byte == 0x02).unwrap();
        assert!(plain[end + 1..].iter().all(|byte| *byte == 0));
        plain[..end].to_vec()
    }

    #[test]
    fn a_message_round_trips_and_every_message_of_a_size_looks_the_same() {
        let rng = rand::SystemRandom::new();
        let receiver =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let ua_public = receiver.compute_public_key().unwrap().as_ref().to_vec();
        let auth = [7u8; AUTH_LEN];
        let short = encrypt(&ua_public, &auth, br#"{"k":"ask"}"#, 32).unwrap();
        let long = encrypt(&ua_public, &auth, br#"{"k":"stopped"}"#, 32).unwrap();
        assert_eq!(short.len(), long.len(), "padded to one size");
        assert_eq!(
            open(receiver, &ua_public, &auth, &long),
            br#"{"k":"stopped"}"#
        );
        let again = encrypt(&ua_public, &auth, br#"{"k":"stopped"}"#, 32).unwrap();
        assert_ne!(&again[..16], &long[..16], "a fresh salt each time");
        assert_ne!(again, long, "and a fresh sender key");
    }

    #[test]
    fn bad_subscription_keys_and_oversized_messages_are_refused() {
        let auth = [7u8; AUTH_LEN];
        assert_eq!(
            encrypt(&[4u8; 64], &auth, b"x", 32),
            Err(PushCryptoError::BadPublicKey)
        );
        let mut not_a_point = [0u8; PUBLIC_KEY_LEN];
        not_a_point[0] = 4;
        assert_eq!(
            encrypt(&not_a_point, &auth, b"x", 32),
            Err(PushCryptoError::BadPublicKey)
        );
        let ua_public = b64(UA_PUBLIC);
        assert_eq!(
            encrypt(&ua_public, &[1u8; 15], b"x", 32),
            Err(PushCryptoError::BadAuth)
        );
        assert_eq!(
            encrypt(&ua_public, &auth, &[b'x'; 32], 32),
            Err(PushCryptoError::TooLong),
            "no room for the delimiter"
        );
        // The largest body every push service takes: 86-byte header, record,
        // tag. One byte more, or an absurd size, is refused (never overflows).
        let largest = MAX_BODY - HEADER_LEN - TAG_LEN;
        assert_eq!(largest, 3994);
        let body = encrypt(&ua_public, &auth, b"x", largest).unwrap();
        assert_eq!(body.len(), MAX_BODY);
        assert!(
            body.len() - HEADER_LEN < RECORD_SIZE as usize,
            "record below rs"
        );
        for too_large in [largest + 1, RECORD_SIZE as usize - TAG_LEN, usize::MAX] {
            assert_eq!(
                encrypt(&ua_public, &auth, b"x", too_large),
                Err(PushCryptoError::TooLong),
                "{too_large}"
            );
        }
    }

    #[test]
    fn the_vapid_token_names_only_its_audience_and_expiry_and_verifies() {
        let rng = rand::SystemRandom::new();
        let pkcs8 = signature::EcdsaKeyPair::generate_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .unwrap();
        let key = signature::EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8.as_ref(),
            &rng,
        )
        .unwrap();
        let value = vapid_authorization(&key, "https://fcm.googleapis.com", 1_900_000_000).unwrap();
        let rest = value.strip_prefix("vapid t=").unwrap();
        let (token, public) = rest.split_once(", k=").unwrap();
        assert_eq!(b64(public), key.public_key().as_ref());
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value = serde_json::from_slice(&b64(parts[0])).unwrap();
        assert_eq!(header, serde_json::json!({"typ": "JWT", "alg": "ES256"}));
        let claims: serde_json::Value = serde_json::from_slice(&b64(parts[1])).unwrap();
        assert_eq!(
            claims,
            serde_json::json!({"aud": "https://fcm.googleapis.com", "exp": 1_900_000_000}),
            "no contact claim"
        );
        let signed = format!("{}.{}", parts[0], parts[1]);
        signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, b64(public))
            .verify(signed.as_bytes(), &b64(parts[2]))
            .expect("the signature verifies with the advertised key");
    }
}
