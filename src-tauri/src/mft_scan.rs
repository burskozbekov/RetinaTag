//! v1.5.346 — NTFS MFT fast directory enumeration (Everything-style).
//!
//! Walking a 66 k-file library with `walkdir::WalkDir` is ~20-40 s
//! cold on a spinning disk — per-entry `FindFirstFileW` /
//! `FindNextFileW` round-trips are the dominant cost.  The Master
//! File Table is one contiguous structure that already lists every
//! file on an NTFS volume; reading it via `FSCTL_ENUM_USN_DATA` is
//! a handful of bulk reads totalling 1-3 s for the same library.
//!
//! This module exposes a single entry point:
//!
//!   pub fn try_fast_scan(root, is_media) -> Option<Vec<PathBuf>>
//!
//! Returns `Some(paths)` on success.  Caller falls back to WalkDir
//! when `None` is returned, which can happen for any of:
//!   • non-NTFS volume (FAT/exFAT/SMB/UNC),
//!   • requested folder not on a drive-letter mount,
//!   • CreateFile on `\\.\C:` fails (admin-elevated install policy,
//!     antivirus block, etc.),
//!   • FSCTL_ENUM_USN_DATA returns an unexpected error,
//!   • root can't be canonicalised.
//!
//! Non-Windows builds get the no-op stub at the bottom so callers
//! can call `try_fast_scan` regardless of target.

use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
pub fn try_fast_scan(
    root: &Path,
    is_media: impl Fn(&Path) -> bool,
) -> Option<Vec<PathBuf>> {
    windows_impl::try_fast_scan(root, is_media)
}

#[cfg(not(target_os = "windows"))]
pub fn try_fast_scan(
    _root: &Path,
    _is_media: impl Fn(&Path) -> bool,
) -> Option<Vec<PathBuf>> {
    None
}

// ─────────────────────────────────────────────────────────────────────
// Windows implementation
// ─────────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use windows::core::HSTRING;
    use windows::Win32::Foundation::*;
    use windows::Win32::Storage::FileSystem::*;
    use windows::Win32::System::Ioctl::*;
    use windows::Win32::System::IO::DeviceIoControl;

    // v1.5.378 — An NTFS file reference is 64-bit: the LOW 48 bits are the
    // MFT record index, the HIGH 16 bits are the record's sequence number.
    // USN_RECORD_V2 reports both FileReferenceNumber and
    // ParentFileReferenceNumber in this packed form.  We index entries and
    // walk parent links by record index only, so the high sequence bits MUST
    // be masked off — otherwise a top-level item's parent (the root dir,
    // record 5) reads as `(seq<<48)|5`, never equals the `== 5` root sentinel,
    // its lookup misses (the root record isn't enumerated), and EVERY path
    // resolution fails → 0 files found on a full library.  This was the
    // "rescan imports nothing / fotolarım nerede" bug.
    const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    /// One MFT entry indexed by file reference number.
    struct Entry {
        name:           String,
        parent_ref:     u64,
        is_directory:   bool,
    }

    pub fn try_fast_scan(
        root: &Path,
        is_media: impl Fn(&Path) -> bool,
    ) -> Option<Vec<PathBuf>> {
        let start = Instant::now();

        // Drive letter sniff.  UNC paths (`\\server\share`) and
        // mapped network drives both fail `\\.\<letter>:` open, so
        // only proceed when root looks like `D:\…`.
        let root_str = root.to_string_lossy();
        let chars: Vec<char> = root_str.chars().collect();
        if chars.len() < 2 || chars[1] != ':' {
            return None;
        }
        let drive_letter = chars[0].to_ascii_uppercase();
        if !drive_letter.is_ascii_alphabetic() {
            return None;
        }

        // Canonical lowercase path for the prefix filter below.  We
        // canonicalize the requested root once so symlinks / `..`
        // segments don't slip past the filter and pull in items
        // outside the scope the user asked for.
        let root_canon = root.canonicalize().ok()?;
        let root_norm = {
            let s = root_canon.to_string_lossy().to_string();
            // canonicalize on Windows often prepends `\\?\` — strip
            // it so the comparison against MFT-derived plain paths
            // works.
            let s = s.trim_start_matches(r"\\?\").to_string();
            s.to_lowercase().trim_end_matches('\\').to_string()
        };

        let entries = enumerate_volume(drive_letter)?;
        eprintln!(
            "[mft] enumerated {} entries on {}: in {:.2}s",
            entries.len(), drive_letter, start.elapsed().as_secs_f32()
        );

        let drive_root = format!("{}:\\", drive_letter);
        let mut out: Vec<PathBuf> = Vec::with_capacity(entries.len() / 4);
        let mut path_buf = String::with_capacity(260);

        for (fref, e) in &entries {
            if e.is_directory { continue; }
            // Resolve full path into `path_buf` (reused across iters
            // so we don't churn millions of String allocations).
            path_buf.clear();
            if !resolve_path_into(&entries, *fref, &drive_root, &mut path_buf) {
                continue;
            }
            // Prefix filter — must live under root.  Lowercase the
            // candidate just enough to compare with `root_norm`.
            if !starts_with_ci(&path_buf, &root_norm) { continue; }
            let pb = PathBuf::from(&path_buf);
            if !is_media(&pb) { continue; }
            out.push(pb);
        }
        eprintln!(
            "[mft] {} media files under {} in {:.2}s",
            out.len(), root.display(), start.elapsed().as_secs_f32()
        );
        // v1.5.378 — Defense-in-depth: if the volume clearly holds files but we
        // resolved ZERO under the requested root, path reconstruction failed
        // (not a genuinely empty folder).  Return None so the caller falls back
        // to the reliable WalkDir walk instead of silently importing nothing —
        // the exact failure mode that hid the user's photos.
        if out.is_empty() && entries.len() > 1000 {
            eprintln!(
                "[mft] 0 paths resolved from {} entries — falling back to WalkDir",
                entries.len()
            );
            return None;
        }
        Some(out)
    }

    /// Open a raw volume handle and enumerate every record in the
    /// MFT via FSCTL_ENUM_USN_DATA, returning a map keyed by file
    /// reference number.
    fn enumerate_volume(letter: char) -> Option<HashMap<u64, Entry>> {
        let volume_path = format!(r"\\.\{}:", letter);
        let volume_path_wide = HSTRING::from(volume_path);

        // Volume handle: read-only.  Some older docs say you need
        // GENERIC_WRITE for FSCTL_ENUM_USN_DATA, but on every
        // Windows 10/11 we tested READ + FILE_SHARE_READ|WRITE is
        // enough.  No admin elevation needed for the read-only
        // path either.
        let handle: HANDLE = match unsafe {
            CreateFileW(
                &volume_path_wide,
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        } {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[mft] CreateFileW {}: {}", letter, e);
                return None;
            }
        };
        if handle.is_invalid() {
            return None;
        }

        // Estimate: most photo libraries on a 1-2 TB volume have
        // 200 k - 2 M MFT entries.  Pre-reserve 500 k so we don't
        // rehash 5+ times during the loop.
        let mut entries: HashMap<u64, Entry> = HashMap::with_capacity(500_000);

        // 1 MiB IO buffer — Windows fills it with as many USN
        // records as fit, then returns.  Bigger doesn't help (the
        // driver paginates internally).
        const BUF_SIZE: usize = 1 << 20;
        let mut buf = vec![0u8; BUF_SIZE];

        let mut next_ref: u64 = 0;
        let mut loop_count: u32 = 0;
        loop {
            loop_count += 1;
            if loop_count > 10_000 {
                // Sanity bound — a 1 MiB buffer holds ~6 k records,
                // so 10 k loops = ~60 M records. We never expect a
                // library that large, treat as runaway.
                eprintln!("[mft] runaway loop after 10000 iterations, bailing");
                break;
            }
            let med = MFT_ENUM_DATA_V0 {
                StartFileReferenceNumber: next_ref,
                LowUsn:  0,
                HighUsn: i64::MAX,
            };
            let mut bytes_returned: u32 = 0;
            let res = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_ENUM_USN_DATA,
                    Some(&med as *const _ as _),
                    std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                    Some(buf.as_mut_ptr() as _),
                    BUF_SIZE as u32,
                    Some(&mut bytes_returned),
                    None,
                )
            };
            if let Err(e) = res {
                // ERROR_HANDLE_EOF (38) is the clean stop sentinel.
                // Anything else, log and return what we've got so
                // we still benefit from the partial enumeration.
                if e.code().0 as u32 != 38 {
                    eprintln!("[mft] DeviceIoControl({}): {}", letter, e);
                }
                break;
            }
            if bytes_returned < 8 { break; }

            // First 8 bytes of the returned buffer is the cursor
            // for the next call.
            next_ref = u64::from_le_bytes(buf[..8].try_into().unwrap());

            // Walk the USN_RECORD_V2 sequence.  RecordLength is the
            // header field that hops to the next record; treat 0 or
            // out-of-bounds as a corrupt stream.
            let mut off = 8usize;
            let total = bytes_returned as usize;
            while off < total {
                let header_ptr = buf[off..].as_ptr() as *const USN_RECORD_V2;
                let rec = unsafe { &*header_ptr };
                let rec_len = rec.RecordLength as usize;
                if rec_len == 0 || off + rec_len > total {
                    break;
                }

                let name_off = rec.FileNameOffset as usize;
                let name_bytes = rec.FileNameLength as usize;
                if name_off + name_bytes <= rec_len {
                    let name_ptr = unsafe {
                        (buf[off..].as_ptr()).add(name_off) as *const u16
                    };
                    let name_chars = name_bytes / 2;
                    let name_slice = unsafe {
                        std::slice::from_raw_parts(name_ptr, name_chars)
                    };
                    let name = String::from_utf16_lossy(name_slice);
                    let is_dir = (rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0;
                    entries.insert(
                        rec.FileReferenceNumber & REF_MASK,
                        Entry {
                            name,
                            parent_ref: rec.ParentFileReferenceNumber & REF_MASK,
                            is_directory: is_dir,
                        },
                    );
                }
                off += rec_len;
            }
        }

        unsafe { let _ = CloseHandle(handle); }
        Some(entries)
    }

    /// Build the full Windows path of `start` by walking parent_ref
    /// links up to the volume root (file ref 5).  Returns false if
    /// the chain is broken or exceeds depth limit.  Writes into
    /// `out` to avoid per-call String allocation in the hot loop.
    fn resolve_path_into(
        entries: &HashMap<u64, Entry>,
        start: u64,
        drive_root: &str,
        out: &mut String,
    ) -> bool {
        // First, collect names walking up.  Plain Vec — depth is
        // typically <10 so the allocation is dwarfed by the MFT
        // enumeration cost.
        let mut parts: Vec<&str> = Vec::with_capacity(16);
        let mut cur = start;
        for _ in 0..128 {
            if cur == 5 { break; }     // 5 = root directory
            match entries.get(&cur) {
                Some(e) => {
                    parts.push(e.name.as_str());
                    if e.parent_ref == cur { break; }   // self-loop guard
                    cur = e.parent_ref;
                }
                None => return false,
            }
        }
        if parts.is_empty() { return false; }
        out.push_str(drive_root);
        for (i, p) in parts.iter().rev().enumerate() {
            if i > 0 { out.push('\\'); }
            out.push_str(p);
        }
        true
    }

    /// Case-insensitive ASCII prefix test.  Both inputs are paths;
    /// the haystack hasn't been lowercased so we compare byte-wise
    /// with a per-char `to_ascii_lowercase`.  Non-ASCII bytes
    /// (Turkish ç, ş, …) pass through as-is since `root_norm` was
    /// already lowercased uniformly.
    fn starts_with_ci(haystack: &str, needle_lc: &str) -> bool {
        let hb = haystack.as_bytes();
        let nb = needle_lc.as_bytes();
        if hb.len() < nb.len() { return false; }
        for (a, b) in hb[..nb.len()].iter().zip(nb.iter()) {
            if a.to_ascii_lowercase() != *b { return false; }
        }
        // Ensure boundary match — either haystack is exactly the
        // needle, or the next byte is a path separator.
        if hb.len() == nb.len() { return true; }
        matches!(hb[nb.len()], b'\\' | b'/')
    }
}
