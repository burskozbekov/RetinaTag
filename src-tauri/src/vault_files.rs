// v1.5.66 — File-level vault encryption.
//
// Up to v1.5.65 the "vault" only encrypted thumbnails — the original
// photo at `photos.path` stayed on disk in plaintext, which meant
// Windows Explorer / any other tool could open it. The user (correctly)
// flagged this as inadequate. This module does the real thing: each
// photo flipped private gets its bytes sealed under the in-memory KEK
// and rewritten as `<original>.rtenc`. The plaintext file is then
// removed.
//
// File format (header is fixed-size so we can stream-decrypt later):
//
//   offset  size  meaning
//   ──────  ────  ──────────────────────────────────────────────
//        0     4  ASCII "RTNT" magic
//        4     1  version byte (currently 0x01)
//        5     3  reserved, zero — bumped to a real flag field if
//                 we ever switch to chunked / multipart encryption
//        8    12  AES-GCM nonce
//       20     N  ciphertext (includes 16-byte auth tag at the end)
//
// Threat model: the .rtenc file alone is useless without the KEK.
// The KEK only lives in `AppState.vault_kek` while the vault is
// unlocked. The KEK itself only escapes the disk wrapped in
// `pin_blob` (PIN-derived) or `recovery_blob` (BIP39-derived) or
// `bio_blob` (DPAPI). So "open the file in Explorer without the PIN"
// reduces to "break AES-256-GCM" — not happening.
//
// Atomicity: we always go orig → temp → fsync → rename → delete.
// A crash before the final rename leaves the orig intact and a stray
// .tmp file. A crash after the rename but before the delete leaves
// BOTH the orig and the .rtenc; on next vault unlock we resolve the
// duplicate by trusting the .rtenc (already-encrypted, atomically
// committed) and removing the leftover orig. See `cleanup_partial`.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::vault_crypto;

const MAGIC: &[u8; 4] = b"RTNT";
const VERSION: u8 = 0x01;
const HEADER_LEN: usize = 4 + 1 + 3 + 12;
pub const RTENC_EXTENSION: &str = "rtenc";

/// Suffix we append to the original path to get the encrypted file's
/// path. We piggyback on the original extension (so `IMG_001.jpg`
/// becomes `IMG_001.jpg.rtenc`) — this keeps the original name visible
/// for housekeeping and survives a hash-of-content rename.
pub fn encrypted_path_for(original: &Path) -> PathBuf {
    let mut s = original.as_os_str().to_os_string();
    s.push(".");
    s.push(RTENC_EXTENSION);
    PathBuf::from(s)
}

/// Reverse of `encrypted_path_for`. Strips the `.rtenc` suffix.
/// Returns None if the path doesn't end in `.rtenc`.
pub fn original_path_for(encrypted: &Path) -> Option<PathBuf> {
    let s = encrypted.to_string_lossy();
    s.strip_suffix(&format!(".{}", RTENC_EXTENSION))
        .map(PathBuf::from)
}

/// True iff the path looks like one of our encrypted blobs (by extension).
pub fn is_encrypted_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case(RTENC_EXTENSION))
        .unwrap_or(false)
}

/// Encrypt the file at `original_path`: writes `<original_path>.rtenc`
/// atomically and returns the resulting `.rtenc` path. **As of v1.5.154
/// this function does NOT delete the original** — that is the caller's
/// job, and it must happen AFTER the DB row has been committed.
///
/// Rationale: pre-1.5.154 this function deleted the source as soon as
/// the `.rtenc` was on disk. A silent DB-commit failure downstream
/// (e.g. `let _ = conn.execute(UPDATE ... private=1)`) then meant the
/// original was gone, the encrypted blob was on disk, but no row
/// tracked it → the photo was effectively lost. Mirror of Mac's v1.5.159
/// fix.
///
/// Errors:
/// - `original_path` doesn't exist or isn't a regular file
/// - `<original_path>.rtenc` already exists (we never silently overwrite
///   pre-existing encrypted blobs — caller must clean up first)
/// - I/O failure
/// - AES-GCM seal failure (RNG starvation, basically never)
pub fn encrypt_in_place(original_path: &Path, kek: &[u8; 32]) -> Result<PathBuf, String> {
    if !original_path.is_file() {
        return Err(format!("not a regular file: {}", original_path.display()));
    }
    let enc_path = encrypted_path_for(original_path);
    if enc_path.exists() {
        return Err(format!(
            "encrypted output already exists: {}",
            enc_path.display()
        ));
    }
    let plaintext = fs::read(original_path)
        .map_err(|e| format!("read {}: {}", original_path.display(), e))?;
    // `vault_crypto::seal` returns nonce ‖ ciphertext+tag.
    let sealed = vault_crypto::seal(kek, &plaintext)?;
    if sealed.len() < 12 + 16 {
        return Err("seal produced impossibly small output".into());
    }
    // Re-frame into our header so callers can identify the file.
    let mut framed = Vec::with_capacity(HEADER_LEN + sealed.len() - 12);
    framed.extend_from_slice(MAGIC);
    framed.push(VERSION);
    framed.extend_from_slice(&[0u8; 3]);
    framed.extend_from_slice(&sealed); // nonce + ciphertext + tag

    write_atomic(&enc_path, &framed)?;
    // v1.5.154 — Caller commits DB row first, then deletes the
    // original via remove_file_with_fallback. If the DB commit fails
    // the caller rolls back by deleting the rtenc instead.
    Ok(enc_path)
}

/// Decrypt `.rtenc` to a plaintext file at `dest_path`. **As of v1.5.154
/// this function does NOT delete the encrypted source** — caller's
/// job after DB commit. Same atomicity reason as `encrypt_in_place`.
pub fn decrypt_to_file(
    encrypted_path: &Path,
    dest_path: &Path,
    kek: &[u8; 32],
) -> Result<(), String> {
    let plaintext = decrypt_to_bytes(encrypted_path, kek)?;
    if dest_path.exists() {
        return Err(format!(
            "destination already exists, refusing to overwrite: {}",
            dest_path.display()
        ));
    }
    write_atomic(dest_path, &plaintext)?;
    Ok(())
}

/// v1.5.173 — Move a sealed `.rtenc` from wherever encrypt_in_place
/// wrote it to its long-term home in the central vault-store dir.
/// Tries `fs::rename` first (atomic on the same volume); if that
/// fails (most often because src and dst are on different volumes —
/// Windows returns ERROR_NOT_SAME_DEVICE = 17), falls back to
/// copy+delete. Copy preserves the bytes 1:1; the sealed envelope's
/// auth tag stays intact, so decrypt still works against the moved
/// blob.
///
/// On success: returns Ok(()), src is gone, dst exists with same
/// bytes. On failure: leaves whatever state the FS gave us — caller
/// decides whether to roll back the DB write.
pub fn move_to_store(src: &Path, dst: &Path) -> Result<(), String> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(src, dst).map_err(|e| {
                format!("copy {} -> {}: {}", src.display(), dst.display(), e)
            })?;
            fs::remove_file(src).map_err(|e| {
                format!("remove src {} after copy: {}", src.display(), e)
            })?;
            Ok(())
        }
    }
}

/// v1.5.154 — Best-effort file removal with a real error on failure.
/// Replaces patterns like `let _ = fs::remove_file(...)` that silently
/// swallowed permission / sharing-violation errors and left orphan
/// .rtenc files on disk after a vault op. The caller decides what to
/// do with the error (most surface it as a per-photo failure but
/// keep the batch going).
pub fn remove_file_with_fallback(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {}", path.display(), e)),
    }
}

/// Read & decrypt the entire encrypted blob into a Vec. Used by the
/// vault lightbox path which ships bytes through IPC as base64. Errors
/// if the file isn't a valid `.rtenc` (wrong magic, wrong version,
/// auth-tag mismatch).
pub fn decrypt_to_bytes(encrypted_path: &Path, kek: &[u8; 32]) -> Result<Vec<u8>, String> {
    let mut f = fs::File::open(encrypted_path)
        .map_err(|e| format!("open {}: {}", encrypted_path.display(), e))?;
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header)
        .map_err(|e| format!("short header in {}: {}", encrypted_path.display(), e))?;
    if &header[0..4] != MAGIC {
        return Err(format!(
            "{} is not a RetinaTag vault file (bad magic)",
            encrypted_path.display()
        ));
    }
    if header[4] != VERSION {
        return Err(format!(
            "{} uses unsupported vault format version {}",
            encrypted_path.display(),
            header[4]
        ));
    }
    // Body is `nonce ‖ ciphertext+tag`. `vault_crypto::open` re-splits.
    // v1.5.74 — Was: `let _ = f.read_to_end(...)` silently dropped short
    // reads / disk errors and let the downstream AES-GCM open() fail with
    // a confusing "auth tag mismatch" / "vault corrupt" message. Users
    // would panic and run recovery, potentially overwriting a salvageable
    // vault. Now we surface the read error verbatim so support can tell
    // disk problems apart from real tag mismatches.
    let mut body = Vec::new();
    f.read_to_end(&mut body).map_err(|e| {
        format!(
            "{}: disk read error ({}). Check the drive — this is NOT a vault corruption.",
            encrypted_path.display(),
            e
        )
    })?;
    let mut nonce_and_ct = Vec::with_capacity(12 + body.len());
    nonce_and_ct.extend_from_slice(&header[8..20]); // the nonce
    nonce_and_ct.extend_from_slice(&body);
    vault_crypto::open(kek, &nonce_and_ct)
}

/// Write `bytes` to `path` atomically: writes to `<path>.tmp`, fsyncs,
/// renames over `<path>`. The temp file is removed on any error.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| format!("create {}: {}", tmp.display(), e))?;
        f.write_all(bytes)
            .map_err(|e| {
                let _ = fs::remove_file(&tmp);
                format!("write {}: {}", tmp.display(), e)
            })?;
        // fsync so the bytes hit the platter before we rename.
        let _ = f.sync_all();
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("rename {} → {}: {}", tmp.display(), path.display(), e)
    })?;
    Ok(())
}

/// Best-effort cleanup of partially-completed encrypt/decrypt
/// operations from a previous run. Run on vault unlock once.
///
/// Cases:
///  - `<path>.rtenc.tmp` exists: previous encrypt didn't finish — remove the
///    tmp.
///  - `<path>` AND `<path>.rtenc` both exist: previous encrypt finished but
///    didn't get to remove the original — keep the .rtenc, drop the orig.
///  - `<path>.rtenc` only: normal post-encrypt state, nothing to do.
pub fn cleanup_partial(original_path: &Path) {
    let enc_path = encrypted_path_for(original_path);
    let tmp_path = {
        let mut s = enc_path.as_os_str().to_os_string();
        s.push(".tmp");
        PathBuf::from(s)
    };
    if tmp_path.exists() {
        let _ = fs::remove_file(&tmp_path);
    }
    if enc_path.exists() && original_path.exists() {
        let _ = fs::remove_file(original_path);
    }
}
