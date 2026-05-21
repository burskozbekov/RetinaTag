"""
v1.5.221 — One-shot rescue for the user's 3,162 photos stranded in
D:\\Fotograflar\\Unknown\\Unknown\\.

What this does, in order:
  1. Backs up the DB to retina.db.bak-<timestamp> before touching it.
  2. Walks D:\\Fotograflar\\Unknown\\Unknown\\ recursively.
  3. For each file, reads EXIF DateTimeOriginal via Pillow (falling
     back to mtime for files without usable EXIF).
  4. Moves the file into D:\\Fotograflar\\<YYYY>\\<MM-MonthName>\\.
  5. UPDATEs the photos row's path + folder so the gallery, timeline,
     and calendar all reflect the new home immediately when RetinaTag
     restarts.
  6. Removes the now-empty Unknown subtree.

Run with RetinaTag CLOSED. Re-runnable if interrupted — only files
still in Unknown/Unknown/ are touched.
"""
import os, sys, shutil, sqlite3, time, datetime
from pathlib import Path

try:
    from PIL import Image
    from PIL.ExifTags import TAGS
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

# EXIF tag IDs for the date fields, in priority order.
EXIF_DATE_TAGS = (
    0x9003,  # DateTimeOriginal — when the photo was actually shot
    0x9004,  # DateTimeDigitized — usually identical to original
    0x0132,  # DateTime — the metadata-edit time, last resort
)

def read_exif_date(path: Path):
    """Return (year, month) from EXIF, or None if unavailable."""
    try:
        with Image.open(path) as img:
            exif = img.getexif()
            if not exif:
                return None
            for tag_id in EXIF_DATE_TAGS:
                if tag_id in exif:
                    raw = exif[tag_id]
                    if isinstance(raw, bytes):
                        raw = raw.decode('ascii', errors='ignore')
                    raw = str(raw).strip()
                    # Format: "YYYY:MM:DD HH:MM:SS"
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

def main():
    unknown_root = ROOT / 'Unknown' / 'Unknown'
    if not unknown_root.exists():
        # Also try single-level fallback
        alt = ROOT / 'Unknown'
        if not alt.exists():
            print(f'No Unknown folder found under {ROOT}. Nothing to do.')
            return
        unknown_root = alt

    if not DB_PATH.exists():
        print(f'DB not found at {DB_PATH}. Make sure RetinaTag is installed and the path is correct.')
        sys.exit(1)

    # Back up DB
    ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
    backup = DB_PATH.with_name(f'retina.db.bak-{ts}')
    print(f'Backing up DB: {DB_PATH} -> {backup}')
    shutil.copy2(DB_PATH, backup)

    # Collect files
    files = []
    for p in unknown_root.rglob('*'):
        if p.is_file() and p.name.lower() not in ('thumbs.db', '.ds_store'):
            files.append(p)
    files.sort()
    total = len(files)
    print(f'Found {total} files in {unknown_root}')
    if total == 0:
        return

    # Open DB (use timeout to wait for any lingering lock to release)
    conn = sqlite3.connect(str(DB_PATH), timeout=30)
    conn.execute('PRAGMA journal_mode=WAL')
    cur = conn.cursor()

    moved = 0
    mtime_fb = 0
    no_db_match = 0
    failed = 0
    start = time.time()

    for i, src in enumerate(files, 1):
        if i % 100 == 0 or i == total:
            elapsed = time.time() - start
            rate = i / elapsed if elapsed > 0 else 0
            print(f'  {i:5d} / {total} ({100*i//total}%) · moved={moved} mtime={mtime_fb} '
                  f'orphan={no_db_match} fail={failed} · {rate:.1f}/s')

        ext = src.suffix.lower()
        ym = None
        if ext in {'.jpg', '.jpeg', '.png', '.tif', '.tiff', '.heic'}:
            ym = read_exif_date(src)
        used_mtime = False
        if ym is None:
            # mtime fallback (videos and EXIF-less stills)
            try:
                m = src.stat().st_mtime
                dt = datetime.datetime.fromtimestamp(m)
                ym = (dt.year, dt.month)
                used_mtime = True
            except OSError:
                failed += 1
                continue

        y, m = ym
        bucket = ROOT / f'{y:04d}' / f'{m:02d}-{MONTHS[m]}'
        bucket.mkdir(parents=True, exist_ok=True)
        dest = bucket / src.name

        # Collision handling
        if dest.exists():
            try:
                same_size = dest.stat().st_size == src.stat().st_size
            except OSError:
                same_size = False
            if same_size:
                # Treat as duplicate — drop the stranded copy and
                # still point the DB row at the existing destination.
                try:
                    src.unlink()
                except OSError:
                    pass
                # Update DB to point at the existing dest
                rows = cur.execute(
                    'UPDATE photos SET path = ?, folder = ? WHERE path = ?',
                    (str(dest), str(bucket), str(src))
                ).rowcount
                if rows == 0:
                    no_db_match += 1
                else:
                    moved += 1
                if used_mtime:
                    mtime_fb += 1
                continue
            else:
                # Different content — append _1, _2, …
                stem = src.stem
                ext_keep = src.suffix
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
            print(f'  FAIL: {src} -> {e}')
            continue

        # Patch DB row — match by old path (the unique key the app uses).
        rows = cur.execute(
            'UPDATE photos SET path = ?, folder = ? WHERE path = ?',
            (str(dest), str(bucket), str(src))
        ).rowcount
        if rows == 0:
            no_db_match += 1
        moved += 1
        if used_mtime:
            mtime_fb += 1

        if i % 200 == 0:
            conn.commit()

    conn.commit()
    conn.close()

    # Best-effort prune of empty Unknown directories
    try:
        for d in [ROOT / 'Unknown' / 'Unknown', ROOT / 'Unknown']:
            if d.exists():
                # rmdir only removes if empty — exactly what we want
                try:
                    # If any leftovers (e.g. permission denied moves), bail safely
                    for sub in d.rglob('*'):
                        pass
                    d.rmdir()
                    print(f'Removed empty: {d}')
                except OSError as e:
                    print(f'Could not remove {d}: {e}')
    except Exception:
        pass

    elapsed = time.time() - start
    print()
    print('===== Done =====')
    print(f'Total files:     {total}')
    print(f'Moved:           {moved}')
    print(f'  via EXIF:      {moved - mtime_fb}')
    print(f'  via mtime:     {mtime_fb}')
    print(f'No DB match:     {no_db_match}  (files were on disk but never indexed)')
    print(f'Failed:          {failed}')
    print(f'Elapsed:         {elapsed:.1f}s')
    print(f'DB backup:       {backup}')

if __name__ == '__main__':
    main()
