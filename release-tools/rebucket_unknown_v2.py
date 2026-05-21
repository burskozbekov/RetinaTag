"""
v1.5.221 — Rescue + repair pass for Unknown/Unknown.

This is v2 of the rebucket script. Handles three states left over from
the partial v1 run:

  A. File still in Unknown/Unknown AND DB row still points there:
     move the file (EXIF or mtime), UPDATE photos.path/folder.

  B. DB row still says Unknown/Unknown but file already moved:
     search the YYYY/MM-Month/ buckets for a file with the same name
     and patch the DB row to point at it.

  C. File in Unknown/Unknown but no DB row:
     just move it (no DB update needed — the next library scan will
     pick it up).

UNIQUE constraint on photos.path is handled by DELETing the orphan
Unknown row when another row already owns the destination path —
the photo lives in the library under that other row, so we drop
the duplicate Unknown registration.

Run with RetinaTag CLOSED. Re-runnable.
"""
import os, sys, shutil, sqlite3, time, datetime
from pathlib import Path

try:
    from PIL import Image
except ImportError:
    print("Pillow not installed. Install with: pip install Pillow")
    sys.exit(1)

ROOT = Path(r'D:\Fotograflar')
DB_PATH = Path(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')

MONTHS = {
    1: 'January', 2: 'February', 3: 'March',     4: 'April',
    5: 'May',     6: 'June',     7: 'July',      8: 'August',
    9: 'September', 10: 'October', 11: 'November', 12: 'December',
}

EXIF_DATE_TAGS = (0x9003, 0x9004, 0x0132)

def read_exif_date(path: Path):
    """Return (year, month) from EXIF, or None."""
    try:
        with Image.open(path) as img:
            exif = img.getexif()
            if not exif:
                return None
            # iPhone EXIF lives in the ExifIFD sub-IFD (tag 0x8769),
            # not the root tags. Check both.
            ifd = None
            try:
                ifd = exif.get_ifd(0x8769)
            except Exception:
                pass
            sources = [exif]
            if ifd:
                sources.append(ifd)
            for src in sources:
                for tag_id in EXIF_DATE_TAGS:
                    if tag_id in src:
                        raw = src[tag_id]
                        if isinstance(raw, bytes):
                            raw = raw.decode('ascii', errors='ignore')
                        raw = str(raw).strip()
                        if len(raw) >= 10 and raw[4] == ':' and raw[7] == ':':
                            try:
                                y = int(raw[0:4]); m = int(raw[5:7])
                                if 1980 <= y <= 2100 and 1 <= m <= 12:
                                    return (y, m)
                            except ValueError:
                                pass
    except Exception:
        pass
    return None

def bucket_for_file(path: Path):
    """(year, month, used_mtime) for a file, EXIF then mtime."""
    used_mtime = False
    ym = None
    if path.suffix.lower() in {'.jpg', '.jpeg', '.png', '.tif', '.tiff', '.heic'}:
        ym = read_exif_date(path)
    if ym is None:
        try:
            m = path.stat().st_mtime
            dt = datetime.datetime.fromtimestamp(m)
            ym = (dt.year, dt.month)
            used_mtime = True
        except OSError:
            return None
    y, m = ym
    return (y, m, used_mtime)

def bucket_dir(y: int, m: int) -> Path:
    return ROOT / f'{y:04d}' / f'{m:02d}-{MONTHS[m]}'

def commit_safe(conn, cur, old_path: str, new_path: Path, new_folder: Path) -> str:
    """
    Try UPDATE; on UNIQUE collision, DELETE the orphan row.
    Returns one of: 'updated', 'dropped-dup', 'no-match'.
    """
    try:
        rows = cur.execute(
            'UPDATE photos SET path = ?, folder = ? WHERE path = ?',
            (str(new_path), str(new_folder), old_path)
        ).rowcount
        return 'updated' if rows > 0 else 'no-match'
    except sqlite3.IntegrityError:
        cur.execute('DELETE FROM photos WHERE path = ?', (old_path,))
        return 'dropped-dup'

def main():
    if not DB_PATH.exists():
        print(f'DB not found at {DB_PATH}')
        sys.exit(1)

    # Back up DB (in addition to v1's backup so we have two snapshots)
    ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
    backup = DB_PATH.with_name(f'retina.db.bak-{ts}')
    print(f'Backing up DB: {DB_PATH} -> {backup}')
    shutil.copy2(DB_PATH, backup)

    conn = sqlite3.connect(str(DB_PATH), timeout=60)
    conn.execute('PRAGMA journal_mode=WAL')
    cur = conn.cursor()

    # Phase 1: repair DB rows whose path is in Unknown but file is gone.
    print()
    print('Phase 1: repairing DB rows whose file has already moved…')
    orphan_db_rows = list(cur.execute(
        "SELECT path FROM photos WHERE path LIKE '%\\Unknown\\Unknown\\%'"
    ))
    repaired = 0
    repair_fail = 0
    repair_dup = 0
    # Pre-index existing YYYY/MM/ files by filename so we can look up O(1)
    print('  Indexing already-moved files for lookup…')
    fn_index = {}
    for year_dir in ROOT.iterdir():
        if not year_dir.is_dir():
            continue
        name = year_dir.name
        if not (len(name) == 4 and name.isdigit()):
            continue
        for month_dir in year_dir.iterdir():
            if not month_dir.is_dir():
                continue
            for f in month_dir.iterdir():
                if f.is_file():
                    # Prefer the most-recently-modified candidate on
                    # collisions — handles the _1 suffix dance.
                    cur_best = fn_index.get(f.name)
                    if cur_best is None or f.stat().st_mtime > cur_best.stat().st_mtime:
                        fn_index[f.name] = f
    print(f'  {len(fn_index):,} indexed files across YYYY buckets')

    for (old_path_str,) in orphan_db_rows:
        old_path = Path(old_path_str)
        if old_path.exists():
            continue  # File still in Unknown — Phase 2 handles it
        # Look up by filename
        target = fn_index.get(old_path.name)
        if target is None:
            repair_fail += 1
            continue
        result = commit_safe(conn, cur, old_path_str, target, target.parent)
        if result == 'updated':
            repaired += 1
        elif result == 'dropped-dup':
            repair_dup += 1
        else:
            repair_fail += 1
    conn.commit()
    print(f'  Phase 1 done: {repaired} repaired, {repair_dup} dropped-as-dup, {repair_fail} unresolved')

    # Phase 2: move remaining files in Unknown/Unknown.
    print()
    print('Phase 2: moving remaining files in Unknown/Unknown…')
    unknown_root = ROOT / 'Unknown' / 'Unknown'
    if not unknown_root.exists():
        unknown_root = ROOT / 'Unknown'
    if not unknown_root.exists():
        print('  No Unknown folder left — done!')
    else:
        files = sorted(p for p in unknown_root.rglob('*')
                       if p.is_file() and p.name.lower() not in ('thumbs.db', '.ds_store'))
        total = len(files)
        print(f'  Found {total} files')
        moved = 0
        no_db = 0
        db_dup = 0
        failed = 0
        mtime_fb = 0
        start = time.time()
        for i, src in enumerate(files, 1):
            if i % 100 == 0 or i == total:
                el = time.time() - start
                rate = i / el if el > 0 else 0
                print(f'  {i:5d} / {total} ({100*i//total}%) · moved={moved} dup={db_dup} '
                      f'orphan-disk={no_db} fail={failed} · {rate:.1f}/s')
            info = bucket_for_file(src)
            if info is None:
                failed += 1
                continue
            y, m, used_mtime = info
            if used_mtime:
                mtime_fb += 1
            bucket = bucket_dir(y, m)
            bucket.mkdir(parents=True, exist_ok=True)
            dest = bucket / src.name

            if dest.exists():
                try:
                    same_size = dest.stat().st_size == src.stat().st_size
                except OSError:
                    same_size = False
                if same_size:
                    try:
                        src.unlink()
                    except OSError:
                        pass
                    # Point DB at the existing destination
                    result = commit_safe(conn, cur, str(src), dest, bucket)
                    if result == 'updated':
                        moved += 1
                    elif result == 'dropped-dup':
                        db_dup += 1
                    else:
                        no_db += 1
                    continue
                # Append _1, _2, …
                stem, ext_keep = src.stem, src.suffix
                n = 1
                while n < 10000:
                    candidate = bucket / f'{stem}_{n}{ext_keep}'
                    if not candidate.exists():
                        dest = candidate
                        break
                    n += 1
            try:
                shutil.move(str(src), str(dest))
            except OSError as e:
                failed += 1
                continue
            result = commit_safe(conn, cur, str(src), dest, bucket)
            if result == 'updated':
                moved += 1
            elif result == 'dropped-dup':
                db_dup += 1
            else:
                # File moved but DB has no row matching its old Unknown
                # path — it was an orphan-on-disk. No DB update needed.
                no_db += 1
            if i % 200 == 0:
                conn.commit()
        conn.commit()

        el = time.time() - start
        print(f'  Phase 2 done in {el:.1f}s: {moved} moved+DBupdated, {db_dup} dup-dropped, '
              f'{no_db} orphans-on-disk, {failed} failed (mtime fallback used: {mtime_fb})')

    # Prune empty Unknown subtree
    for d in [ROOT / 'Unknown' / 'Unknown', ROOT / 'Unknown']:
        if d.exists():
            try:
                # Only remove if truly empty (including empty subdirs)
                shutil.rmtree(d)
                print(f'Removed (was empty): {d}')
            except OSError as e:
                print(f'Could not remove {d}: {e}')

    # Final consistency check
    remaining = cur.execute(
        "SELECT COUNT(*) FROM photos WHERE path LIKE '%\\Unknown\\Unknown\\%' OR path LIKE '%\\Unknown\\%'"
    ).fetchone()[0]
    conn.close()
    print()
    print(f'DB rows still containing "Unknown" in path: {remaining}')
    if remaining == 0:
        print('CLEAN — every photo now lives in its real YYYY/MM-Month/ bucket.')
    print(f'DB backup at: {backup}')

if __name__ == '__main__':
    main()
