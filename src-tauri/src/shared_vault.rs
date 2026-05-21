//! v1.5.222 — Shared SMB vault.
//!
//! A vault format that Mac and Windows agree on byte-for-byte so a
//! library kept on a shared SMB volume (NAS, Time Machine, sparse
//! bundle, anything that presents a normal filesystem) can be
//! unlocked from either platform with the same PIN.
//!
//! Layout under `<library_root>/.retinatag-vault/`:
//!
//!   salt.bin          16 bytes, written once at first setup
//!   wrapped-key.bin   60 bytes: 12-byte nonce ‖ AES-256-GCM(KEK, master_key) + 16-byte tag
//!   meta.json         { "version": 1, "created_at": "...", "created_by": "mac|pc" }
//!   objects/
//!     <oid[0..2]>/
//!       <oid>.rtenc   encrypted photo blob
//!
//! `.rtenc` byte layout (single AES-GCM segment, no inner JSON header):
//!
//!   [ 0.. 4]  magic     = b"RT01"
//!   [ 4.. 5]  version   = 0x01
//!   [ 5.. 8]  reserved  = 0x00 0x00 0x00
//!   [ 8..20]  nonce     (12 random bytes from OsRng)
//!   [20.. ..] AES-256-GCM(master_key, nonce, file_bytes) + 16-byte tag
//!
//! `oid` = SHA-256(file_bytes), lowercase hex (64 chars). Same input
//! produces the same oid on both platforms, so a photo added from
//! Mac and the same photo added from PC collapse into one blob —
//! no duplicates, no race.
//!
//! Filename / path / media type / capture date are NOT inside the
//! encrypted blob. They live in the per-machine `photos` table
//! (and propagate cross-machine via the `retinatag:VaultOid` XMP
//! sidecar that Phase 2 will land).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::vault_crypto;

// ─── Format constants ──────────────────────────────────────────────────────

/// Magic prefix on every `.rtenc` so we can tell shared-vault blobs apart
/// from anything else on disk at a glance.
pub const MAGIC: &[u8; 4] = b"RT01";

/// Format version. Anything other than 1 in this byte means a future
/// version we don't understand — we refuse to read it.
pub const VERSION: u8 = 0x01;

/// Total header length before the GCM ciphertext: magic(4) + version(1)
/// + reserved(3) + nonce(12) = 20 bytes.
pub const HEADER_LEN: usize = 20;

/// Length of the wrapped-key.bin file: 12-byte nonce + 32-byte master
/// key sealed under AES-GCM (which appends a 16-byte tag) = 60 bytes.
pub const WRAPPED_KEY_LEN: usize = 12 + 32 + 16;

/// Vault directory name. Hidden by leading dot on both macOS and
/// Windows. On Windows we additionally set the +H attribute after
/// creation so Explorer also hides it.
pub const VAULT_DIRNAME: &str = ".retinatag-vault";

// ─── Path helpers ──────────────────────────────────────────────────────────

pub fn vault_dir(library_root: &Path) -> PathBuf {
    library_root.join(VAULT_DIRNAME)
}

fn salt_path(library_root: &Path) -> PathBuf {
    vault_dir(library_root).join("salt.bin")
}

fn wrapped_key_path(library_root: &Path) -> PathBuf {
    vault_dir(library_root).join("wrapped-key.bin")
}

fn meta_path(library_root: &Path) -> PathBuf {
    vault_dir(library_root).join("meta.json")
}

fn objects_dir(library_root: &Path) -> PathBuf {
    vault_dir(library_root).join("objects")
}

/// Resolve the on-disk location of an oid-addressed blob.
///   `<vault>/objects/<oid[0..2]>/<oid>.rtenc`
pub fn object_path(library_root: &Path, oid: &str) -> PathBuf {
    objects_dir(library_root)
        .join(&oid[0..2])
        .join(format!("{}.rtenc", oid))
}

// ─── Setup / probe / unlock ────────────────────────────────────────────────

/// Quick check: is a vault initialised at this library root?
pub fn vault_exists(library_root: &Path) -> bool {
    salt_path(library_root).exists() && wrapped_key_path(library_root).exists()
}

/// Initialise a brand-new shared vault. Generates a fresh salt, derives
/// the KEK from `pin`, generates a random 32-byte master key, wraps it
/// under the KEK, and writes everything to disk atomically. Refuses to
/// run if a vault already exists at this root (you'd lose the master
/// key the existing files were sealed under).
///
/// `created_by` should be `"pc"` on this platform; the field is purely
/// informational, used in the meta.json so a curious user can tell
/// which side initialised the vault.
pub fn init_vault(
    library_root: &Path,
    pin: &str,
    created_by: &str,
) -> Result<(), String> {
    if pin.is_empty() {
        return Err("PIN cannot be empty".to_string());
    }
    if vault_exists(library_root) {
        return Err(format!(
            "vault already exists at {} — use unlock_vault to open it",
            vault_dir(library_root).display()
        ));
    }
    let vdir = vault_dir(library_root);
    fs::create_dir_all(&vdir)
        .map_err(|e| format!("create vault dir {}: {}", vdir.display(), e))?;
    fs::create_dir_all(objects_dir(library_root))
        .map_err(|e| format!("create objects dir: {}", e))?;

    // Salt: 16 random bytes from OsRng.
    let salt = vault_crypto::random_salt();
    write_atomic(&salt_path(library_root), &salt)
        .map_err(|e| format!("write salt.bin: {}", e))?;

    // Master key: 32 random bytes from OsRng.
    let mut master_key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut master_key);

    // Wrap master key under KEK derived from PIN+salt.
    let kek = vault_crypto::derive_kek(pin, &salt)?;
    let wrapped = vault_crypto::seal(&kek, &master_key)?;
    if wrapped.len() != WRAPPED_KEY_LEN {
        return Err(format!(
            "wrapped key wrong size: got {} want {}",
            wrapped.len(),
            WRAPPED_KEY_LEN
        ));
    }
    write_atomic(&wrapped_key_path(library_root), &wrapped)
        .map_err(|e| format!("write wrapped-key.bin: {}", e))?;

    // Meta.json — informational only.
    let now = chrono::Utc::now().to_rfc3339();
    let meta = serde_json::json!({
        "version": 1,
        "created_at": now,
        "created_by": created_by,
    });
    let meta_bytes = serde_json::to_vec_pretty(&meta)
        .map_err(|e| format!("meta serialize: {}", e))?;
    write_atomic(&meta_path(library_root), &meta_bytes)
        .map_err(|e| format!("write meta.json: {}", e))?;

    // Best-effort: mark the vault dir hidden on Windows. POSIX hides it
    // already via the dot prefix.
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::ffi::OsStr;
        // SetFileAttributesW(path, FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_DIRECTORY)
        let wide: Vec<u16> = OsStr::new(&vdir).encode_wide().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = windows::Win32::Storage::FileSystem::SetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(
                    0x02 /* FILE_ATTRIBUTE_HIDDEN */
                ),
            );
        }
    }

    // Hygiene: wipe the master key from this stack frame ASAP. Caller
    // didn't get it from us anyway; setup-then-unlock is the contract.
    master_key.fill(0);
    Ok(())
}

/// Open an existing vault. Derives the KEK from PIN+salt, decrypts the
/// wrapped master key, and returns it. The 32-byte master key is the
/// key callers use directly for `encrypt_blob` / `decrypt_blob`.
///
/// Returns Err("incorrect PIN") on AES-GCM tag failure — the only way
/// the user can tell wrong-PIN from corrupt-vault. (The tag failure
/// is indistinguishable from a tampered ciphertext, which is what we
/// want — don't leak which.)
pub fn unlock_vault(library_root: &Path, pin: &str) -> Result<[u8; 32], String> {
    if !vault_exists(library_root) {
        return Err(format!(
            "no vault at {} — call init_vault first",
            vault_dir(library_root).display()
        ));
    }
    let salt = fs::read(salt_path(library_root))
        .map_err(|e| format!("read salt.bin: {}", e))?;
    if salt.len() != 16 {
        return Err(format!("salt.bin wrong size: {}", salt.len()));
    }
    let wrapped = fs::read(wrapped_key_path(library_root))
        .map_err(|e| format!("read wrapped-key.bin: {}", e))?;
    if wrapped.len() != WRAPPED_KEY_LEN {
        return Err(format!(
            "wrapped-key.bin wrong size: got {} want {}",
            wrapped.len(),
            WRAPPED_KEY_LEN
        ));
    }
    let kek = vault_crypto::derive_kek(pin, &salt)?;
    let master_vec = vault_crypto::open(&kek, &wrapped)
        .map_err(|_| "incorrect PIN".to_string())?;
    if master_vec.len() != 32 {
        return Err(format!(
            "decrypted master key wrong size: {}",
            master_vec.len()
        ));
    }
    let mut master = [0u8; 32];
    master.copy_from_slice(&master_vec);
    Ok(master)
}

// ─── Encrypt / decrypt ─────────────────────────────────────────────────────

/// Compute the oid (SHA-256 hex, lowercase) of a byte slice. Public so
/// callers who already have the bytes in memory can derive the oid
/// without going through the encrypt path.
pub fn oid_of(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Encrypt a file into the shared vault, returning the oid. Re-encrypting
/// the same file content always lands at the same oid, so a second
/// upload is a fast no-op (we keep the existing blob, just return the
/// oid). Atomic temp-then-rename so an interrupted write never leaves
/// a half-baked `.rtenc` on disk for the unlock path to choke on.
pub fn encrypt_file_to_vault(
    library_root: &Path,
    master_key: &[u8; 32],
    src_path: &Path,
) -> Result<String, String> {
    let plain = fs::read(src_path)
        .map_err(|e| format!("read {}: {}", src_path.display(), e))?;
    let oid = oid_of(&plain);
    let dest = object_path(library_root, &oid);
    // If the blob is already there, the existing copy is identical
    // (oid is content-addressed) — no need to re-encrypt.
    if dest.exists() {
        return Ok(oid);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    let blob = encrypt_to_blob(master_key, &plain)?;
    write_atomic(&dest, &blob)
        .map_err(|e| format!("write {}: {}", dest.display(), e))?;
    Ok(oid)
}

/// Build the `.rtenc` byte sequence for a given plaintext. Separated
/// from disk I/O so callers that already have plaintext bytes (e.g.
/// streaming an MTP import in memory) can avoid the temp-file round
/// trip.
pub fn encrypt_to_blob(master_key: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(master_key)
        .map_err(|e| format!("aes key: {}", e))?;
    let nonce_bytes = vault_crypto::random_nonce();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plain)
        .map_err(|e| format!("aes seal: {}", e))?;
    let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
    out.extend_from_slice(MAGIC);                 // 4
    out.push(VERSION);                             // 1
    out.extend_from_slice(&[0u8; 3]);              // 3 reserved
    out.extend_from_slice(&nonce_bytes);           // 12
    out.extend_from_slice(&ct);                    // ciphertext+tag
    Ok(out)
}

/// Decrypt an oid-addressed blob, returning the original bytes.
pub fn decrypt_oid_to_bytes(
    library_root: &Path,
    master_key: &[u8; 32],
    oid: &str,
) -> Result<Vec<u8>, String> {
    let p = object_path(library_root, oid);
    let blob = fs::read(&p)
        .map_err(|e| format!("read {}: {}", p.display(), e))?;
    decrypt_blob(master_key, &blob)
}

/// Decrypt an in-memory `.rtenc` blob. Strict format validation: magic
/// + version must match exactly, and AES-GCM tag failure cleanly errs
/// instead of producing garbage.
pub fn decrypt_blob(master_key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < HEADER_LEN + 16 {
        return Err(format!("blob too short: {}", blob.len()));
    }
    if &blob[0..4] != MAGIC {
        return Err(format!(
            "bad magic: expected {:?}, got {:?}",
            MAGIC,
            &blob[0..4]
        ));
    }
    if blob[4] != VERSION {
        return Err(format!(
            "unsupported version: 0x{:02x}",
            blob[4]
        ));
    }
    let nonce = Nonce::from_slice(&blob[8..20]);
    let cipher = Aes256Gcm::new_from_slice(master_key)
        .map_err(|e| format!("aes key: {}", e))?;
    cipher
        .decrypt(nonce, &blob[HEADER_LEN..])
        .map_err(|e| format!("aes open: {}", e))
}

/// Decrypt an oid-addressed blob to a destination file. Atomic — the
/// destination only appears after the full plaintext is on disk and
/// the AES-GCM tag verified.
pub fn decrypt_oid_to_file(
    library_root: &Path,
    master_key: &[u8; 32],
    oid: &str,
    dest: &Path,
) -> Result<(), String> {
    let plain = decrypt_oid_to_bytes(library_root, master_key, oid)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    write_atomic(dest, &plain).map_err(|e| format!("write {}: {}", dest.display(), e))
}

// ─── Discovery ─────────────────────────────────────────────────────────────

/// List every oid currently sitting in the vault's `objects/` tree.
/// Walks one level of shard subdirs (the `aa/`, `b3/`, …) and reads
/// the filenames; doesn't open the blobs. Useful as a cross-platform
/// "what's in the vault" probe before any DB rows are written.
pub fn list_oids(library_root: &Path) -> Result<Vec<String>, String> {
    let dir = objects_dir(library_root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let shards = fs::read_dir(&dir)
        .map_err(|e| format!("read {}: {}", dir.display(), e))?;
    for shard in shards.flatten() {
        let pt = shard.path();
        if !pt.is_dir() {
            continue;
        }
        let inner = match fs::read_dir(&pt) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for f in inner.flatten() {
            let fp = f.path();
            let name = match fp.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => continue,
            };
            // Expect "<64-hex>.rtenc"
            if !name.ends_with(".rtenc") {
                continue;
            }
            let stem = &name[..name.len() - 6];
            if stem.len() == 64 && stem.chars().all(|c| c.is_ascii_hexdigit()) {
                out.push(stem.to_ascii_lowercase());
            }
        }
    }
    Ok(out)
}

// ─── Atomic file write ────────────────────────────────────────────────────

/// Write `bytes` to `path` via a sibling temp file + rename. Survives
/// power loss, app crash, and the user yanking the SMB share mid-write
/// — the caller either sees the old file or the new file, never a
/// half-written one. Mirrors the pattern `vault_files::write_atomic`
/// uses for legacy per-machine vault writes.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    // Use a temp name that lives next to the final file so the rename
    // is always same-volume (cheap atomic move on NTFS / APFS / SMB).
    let mut tmp = path.to_path_buf();
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no filename",
            )
        })?;
    let pid = std::process::id();
    let nonce = rand::thread_rng().next_u32();
    tmp.set_file_name(format!(".{}.{}-{}.tmp", fname, pid, nonce));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // rename overwrites the destination on Windows when the source is
    // on the same volume — exactly what we want.
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_in_temp_dir() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        init_vault(root, "swordfish", "pc").unwrap();
        assert!(vault_exists(root));
        let mk = unlock_vault(root, "swordfish").unwrap();
        // Wrong PIN must fail.
        assert!(unlock_vault(root, "wrong").is_err());

        // Encrypt + decrypt round trip.
        let src = td.path().join("photo.jpg");
        fs::write(&src, b"hello world, this is a 'photo'").unwrap();
        let oid = encrypt_file_to_vault(root, &mk, &src).unwrap();
        assert_eq!(oid.len(), 64);
        let plain = decrypt_oid_to_bytes(root, &mk, &oid).unwrap();
        assert_eq!(plain, b"hello world, this is a 'photo'");

        // Encrypting the same content again hits the existing-blob fast
        // path and returns the same oid without rewriting.
        let oid2 = encrypt_file_to_vault(root, &mk, &src).unwrap();
        assert_eq!(oid, oid2);

        let oids = list_oids(root).unwrap();
        assert_eq!(oids, vec![oid]);
    }
}
