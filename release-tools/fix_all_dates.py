"""
v1.5.244 — Library-wide date_taken repair.

User has been chasing wrong photo dates for days. Multiple sources
of damage stacked up:
  • Pre-v1.5.232 MTP imports stored WPD's `date_modified` (often
    "today" for Messages photos / re-saved photos) as date_taken.
  • Some files lost EXIF entirely when iOS Sharing / Messages
    re-encoded them (strips DateTimeOriginal).
  • mtime is usually preserved across phone→Windows copies, so it
    can rescue files without EXIF.
  • The Unknown rebucket migration moved files to YYYY/MM-Month/
    based on date_taken, so wrong dates also mean wrong folders.

This script:
  Pass A — Re-derive date_taken for every photo:
    1. Read EXIF DateTimeOriginal / DateTimeDigitized / DateTime
       via Pillow.
    2. If none, fall back to file mtime (preserves capture date
       when copied from phone).
    3. UPDATE photos.date_taken ONLY when:
         a) DB is NULL or empty
         b) DB diverges by > 1 minute from EXIF
         c) DB date is "today-ish" (within 7 days) but EXIF or
            mtime is older — that's the "WPD lied" signature.
  Pass B (summary only — does NOT move files yet): report how many
    photos' year/month folder now disagrees with their canonical
    date_taken, so we know how much rebucket work follows.

Backs up DB first. Run with the app CLOSED.
"""
import sqlite3, sys, io, datetime, shutil, time
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

try:
    from PIL import Image
except ImportError:
    print('Pillow not installed — pip install Pillow'); sys.exit(1)

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-dates-{ts}')
print(f'Backing up DB → {backup.name}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=60)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

EXIF_DATE_TAGS = (0x9003, 0x9004, 0x0132)  # DateTimeOriginal, Digitized, DateTime
IMAGE_EXTS = {'.jpg', '.jpeg', '.png', '.tif', '.tiff', '.heic', '.heif', '.webp'}
TODAY = datetime.date.today()

def parse_db_dt(s):
    """Accept 'YYYY-MM-DD HH:MM:SS' or 'YYYY-MM-DDTHH:MM:SS' or just date."""
    if not s: return None
    s = s.strip()
    for fmt in ('%Y-%m-%d %H:%M:%S', '%Y-%m-%dT%H:%M:%S',
                '%Y-%m-%d %H:%M', '%Y-%m-%d'):
        try: return datetime.datetime.strptime(s[:len(fmt.replace('%Y','9999')
                .replace('%m','99').replace('%d','99').replace('%H','99')
                .replace('%M','99').replace('%S','99'))], fmt)
        except ValueError: continue
    # Last resort, try first 19 chars in canonical shape
    try:
        return datetime.datetime.strptime(s[:19].replace('T',' '), '%Y-%m-%d %H:%M:%S')
    except Exception:
        return None

def read_exif_dt(path):
    p = Path(path)
    if p.suffix.lower() not in IMAGE_EXTS: return None
    try:
        with Image.open(path) as img:
            exif = img.getexif()
            if not exif: return None
            sources = [exif]
            # iPhone EXIF lives in the ExifIFD (tag 0x8769); check both.
            try:
                ifd = exif.get_ifd(0x8769)
                if ifd: sources.append(ifd)
            except Exception:
                pass
            for src in sources:
                for tid in EXIF_DATE_TAGS:
                    if tid in src:
                        raw = src[tid]
                        if isinstance(raw, bytes):
                            raw = raw.decode('ascii', errors='ignore')
                        raw = str(raw).strip()
                        # Format "YYYY:MM:DD HH:MM:SS"
                        if len(raw) >= 19:
                            try:
                                y = int(raw[0:4]); m = int(raw[5:7]); d = int(raw[8:10])
                                hh = int(raw[11:13]); mm = int(raw[14:16]); ss = int(raw[17:19])
                                if 1980 <= y <= 2100 and 1 <= m <= 12 and 1 <= d <= 31:
                                    return datetime.datetime(y, m, d, hh, mm, ss)
                            except ValueError:
                                continue
    except Exception:
        pass
    return None

def file_mtime_dt(path):
    try:
        m = Path(path).stat().st_mtime
        return datetime.datetime.fromtimestamp(m)
    except OSError:
        return None

# Cursor + counters
all_rows = cur.execute("SELECT id, path, date_taken FROM photos").fetchall()
total = len(all_rows)
print(f'Total photos: {total:,}')

fix_from_exif = 0
fix_from_mtime = 0
unchanged = 0
no_file = 0
no_date_at_all = 0
batch = []
start = time.time()
for i, r in enumerate(all_rows, 1):
    if i % 1000 == 0 or i == total:
        el = time.time() - start
        rate = i / el if el > 0 else 0
        eta = (total - i) / rate if rate > 0 else 0
        print(f'  {i:>6}/{total} ({100*i//total}%)  exif={fix_from_exif} '
              f'mtime={fix_from_mtime} unchanged={unchanged} missing={no_file} '
              f'eta={int(eta)}s')
        if batch:
            cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
            conn.commit()
            batch = []
    p = r['path']
    if not Path(p).exists():
        no_file += 1
        continue
    db_dt = parse_db_dt(r['date_taken'])
    exif_dt = read_exif_dt(p)
    mtime_dt = None
    if not exif_dt:
        mtime_dt = file_mtime_dt(p)
    # Pick the canonical value: EXIF beats everything.
    canonical = exif_dt or mtime_dt
    if not canonical:
        no_date_at_all += 1
        continue
    # Decide whether to overwrite.
    should_update = False
    if not db_dt:
        should_update = True
    else:
        delta = abs((canonical - db_dt).total_seconds())
        # 1 minute tolerance for clock-skew noise
        if delta > 60:
            # WPD-lied detector: DB date close to "today-ish" but
            # canonical is older — likely the Messages-video bug
            # where WPD reported save-to-Camera-Roll time.
            days_to_today = abs((TODAY - db_dt.date()).days)
            if exif_dt:
                # EXIF always wins
                should_update = True
            elif days_to_today <= 7 and (db_dt - canonical).total_seconds() > 24*3600:
                # mtime is older by > 1 day AND DB looks like today
                should_update = True
    if should_update:
        new_str = canonical.strftime('%Y-%m-%d %H:%M:%S')
        batch.append((new_str, r['id']))
        if exif_dt:
            fix_from_exif += 1
        else:
            fix_from_mtime += 1
    else:
        unchanged += 1

# Flush remaining
if batch:
    cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
    conn.commit()

el = time.time() - start
print()
print(f'===== Done in {el:.1f}s =====')
print(f'Fixed via EXIF:        {fix_from_exif:,}')
print(f'Fixed via mtime:       {fix_from_mtime:,}')
print(f'Unchanged (DB matched):{unchanged:,}')
print(f'File missing:          {no_file:,}')
print(f'No EXIF + no mtime:    {no_date_at_all:,}')

# Pass B — folder vs. date mismatch report (informational)
print()
print('=== Folder/date mismatch check (info only — no moves) ===')
mismatches = 0
for r in cur.execute(
    "SELECT id, path, date_taken FROM photos "
    "WHERE date_taken IS NOT NULL AND date_taken != ''"
).fetchall():
    dt = parse_db_dt(r['date_taken'])
    if not dt: continue
    parts = Path(r['path']).parts
    # Looking for a "YYYY" segment then "MM-Month" segment
    if len(parts) < 3: continue
    year_seg = None
    for s in parts:
        if len(s) == 4 and s.isdigit() and 1990 <= int(s) <= 2099:
            year_seg = int(s); break
    if year_seg is None: continue
    if year_seg != dt.year:
        mismatches += 1
print(f'Photos in a year-folder that disagrees with their date_taken: {mismatches:,}')
print('(That is rebucket work — separate pass once dates are stable.)')

conn.close()
print()
print(f'DB backup: {backup}')
