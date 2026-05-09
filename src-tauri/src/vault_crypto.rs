// v1.5.63 — Vault Faz 2 cryptography primitives.
//
// Three-layer model:
//
//   1. PIN  ─Argon2id(salt)──▶  KEK (32 B)
//      The PIN never leaves memory. Argon2id is tuned to ≥ 250 ms on a
//      modern laptop so brute-forcing is expensive even with the salt
//      stolen.
//
//   2. Recovery mnemonic (BIP39, 24 words = 256 bits entropy)
//      ─PBKDF2 / Argon2id(fixed salt)──▶  RKEK (32 B)
//      Generated once at PIN setup. Shown to the user exactly once. We
//      never store the mnemonic, only an AES-GCM ciphertext of the KEK
//      under the RKEK so "I forgot my PIN" can recover the master key
//      without ever transmitting it through email or the cloud.
//
//   3. KEK is what actually wraps individual photo thumbnails (Faz 2.1,
//      v1.5.64+). For now we just derive and stash it; the encryption
//      migration is gated on a separate setting so existing libraries
//      keep working.
//
// Threat model recap: a curious roommate borrows the laptop, opens
// RetinaTag, and clicks Vault. The PIN is 6+ chars and not "123456",
// they don't have biometric, and the wrong-PIN backoff (3 fails → 30 s,
// 10 fails → wipe option) caps online attempts. If they image the disk
// and brute-force offline, Argon2id makes that hours-per-PIN, not
// nanoseconds. They cannot read thumbnails because they're AES-GCM'd
// under a key derived from a PIN we don't store.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use bip39::{Language, Mnemonic};
use rand::RngCore;

/// Argon2id parameters. Tuned for ≥ 250 ms on a 2023-era laptop so an
/// offline brute-force on a leaked DB is meaningfully slow.
///   memory: 64 MiB, iterations: 3, parallelism: 1
fn argon2_default() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(64 * 1024, 3, 1, Some(32))
            .expect("argon2 params constants are always valid"),
    )
}

/// Derive a 32-byte KEK from a PIN using Argon2id. The salt comes from
/// the vault row in the DB so re-deriving on unlock produces the same
/// key. NEVER reuse a salt across vaults.
pub fn derive_kek(pin: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let argon = argon2_default();
    let mut out = [0u8; 32];
    argon
        .hash_password_into(pin.as_bytes(), salt, &mut out)
        .map_err(|e| format!("argon2: {}", e))?;
    Ok(out)
}

/// Generate a fresh 16-byte random salt. Use os entropy, not the
/// xorshift `rand_u32` in db.rs — KDF salts must be cryptographically
/// random.
pub fn random_salt() -> [u8; 16] {
    let mut s = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut s);
    s
}

/// Generate a fresh 12-byte random nonce for AES-GCM. AES-GCM is
/// catastrophically broken under nonce reuse — every encrypt operation
/// must generate a new one and store it alongside the ciphertext.
pub fn random_nonce() -> [u8; 12] {
    let mut n = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut n);
    n
}

/// AES-256-GCM seal. Returns nonce ‖ ciphertext+tag concatenated, so
/// callers only need to store one blob. Open() splits it back.
pub fn seal(kek: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(kek).map_err(|e| format!("aes key: {}", e))?;
    let nonce_bytes = random_nonce();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| format!("aes seal: {}", e))?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Reverse of `seal`. Expects nonce ‖ ciphertext+tag. Verifies the
/// AES-GCM tag, so a wrong KEK or tampered ciphertext cleanly returns
/// Err instead of garbage plaintext.
pub fn open(kek: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < 12 + 16 {
        return Err("vault blob too short".into());
    }
    let cipher = Aes256Gcm::new_from_slice(kek).map_err(|e| format!("aes key: {}", e))?;
    let (nonce_bytes, ct) = blob.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ct)
        .map_err(|e| format!("aes open: {}", e))
}

/// Generate a fresh 24-word BIP39 recovery mnemonic. 24 words ≈ 256
/// bits of entropy — the same security level as the underlying KEK,
/// so the recovery path is no weaker than the PIN path.
///
/// We return the joined string ("abandon abandon ... about") and the
/// caller is expected to display it to the user EXACTLY ONCE. We do
/// not log or persist it.
pub fn generate_recovery_mnemonic() -> Result<String, String> {
    let mut entropy = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut entropy);
    let m = Mnemonic::from_entropy_in(Language::English, &entropy)
        .map_err(|e| format!("bip39: {}", e))?;
    Ok(m.to_string())
}

/// Validate that a user-supplied recovery phrase parses as a real BIP39
/// mnemonic in English. Catches typos and "the user pasted gibberish"
/// before we waste cycles trying to derive a key from it.
pub fn validate_mnemonic(phrase: &str) -> Result<(), String> {
    Mnemonic::parse_in(Language::English, phrase.trim())
        .map(|_| ())
        .map_err(|e| format!("invalid recovery phrase: {}", e))
}

/// v1.5.68 — Derive the actual KEK directly from the BIP39 mnemonic.
/// Deterministic: same 24 words on any machine produce the same KEK.
/// That's what makes cross-device portability work — copy the .rtenc
/// files to another machine, type the same words, decrypt them.
///
/// We use a fixed app-wide salt (not per-vault) because:
///   1. The mnemonic already carries 256 bits of entropy; random
///      salting buys no additional security.
///   2. Per-vault salting would defeat portability (you'd need to
///      transport the salt too, which means another secret to manage).
///   3. The fixed salt namespaces our derivation — typing a wallet
///      mnemonic into RetinaTag and into Bitcoin gives different KEKs
///      because they use different fixed salts.
///
/// Earlier versions (≤ v1.5.67) named this `derive_rkek` and used it
/// only for a "recovery wrapping key" around a separately-random KEK.
/// In v1.5.68 the KEK and the mnemonic-derived key are the same thing,
/// which removes a layer and makes portability trivial.
pub fn derive_kek_from_mnemonic(phrase: &str) -> Result<[u8; 32], String> {
    Mnemonic::parse_in(Language::English, phrase.trim())
        .map_err(|e| format!("invalid recovery phrase: {}", e))?;
    // Fixed app salt — bip39 entropy is already cryptographically random,
    // we just need the slow KDF stretching, not extra randomness.
    const APP_SALT: &[u8; 16] = b"RTNTG_VAULT_RKEK";
    let argon = argon2_default();
    let mut out = [0u8; 32];
    argon
        .hash_password_into(phrase.trim().as_bytes(), APP_SALT, &mut out)
        .map_err(|e| format!("argon2 rkek: {}", e))?;
    Ok(out)
}
