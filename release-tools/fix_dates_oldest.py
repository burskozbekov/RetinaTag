"""
v1.5.244 — Library-wide date_taken repair, "oldest wins" policy.

User stated the rule plainly:
  * Use the OLDEST EXIF date available.
  * If no EXIF, fall back to file mtime.
  * If even mtime is gone, use ctime (file creation on this PC).
  * NEVER leave a row without a date — there is always SOMETHING.

So for every photo we collect every candidate timestamp:
   - EXIF DateTimeOriginal  (tag 0x9003)
   - EXIF DateTimeDigitized (tag 0x9004)
   - EXIF DateTime          (tag 0x0132)
   - GPSDateStamp + GPSTimeStamp (combined)
   - File mtime
   - File ctime (Windows: file creation time)
…then pick the EARLIEST (capture is always older than any
re-save / copy), with sanity clamp 1980 ≤ year ≤ 2100. Any value
outside that band is treated as missing.

UPDATE rule: write the canonical date when
  * DB value is NULL/empty, OR
  * canonical differs from DB by > 1 minute AND is OLDER
    (we never make a row "more recent" against the user's wish).

Run with the app CLOSED.
"""
import sqlite3, sys, io, datetime, shutil, time
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

try:
    from PIL import Image
except ImportError:
    print('Pillow required: pip install Pillow'); sys.exit(1)

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

EXIF_DATE_TAGS = (0x9003, 0x9004, 0x0132)
GPS_DATE_TAG   = 29   # GPSDateStamp inside GPSInfo IFD (0x8825)
GPS_TIME_TAG   = 7    # GPSTimeStamp
EXIF_IFD_TAG   = 0x8769
GPS_IFD_TAG    = 0x8825
IMAGE_EXTS = {'.jpg', '.jpeg', '.png', '.tif', '.tiff', '.heic', '.heif', '.webp'}

YEAR_MIN, YEAR_MAX = 1980, 2100

def clamp_dt(dt):
    if dt is None: return None
    if not (YEAR_MIN <= dt.year <= YEAR_MAX): return None
    return dt

def parse_exif_dt_str(raw):
    """Parse 'YYYY:MM:DD HH:MM:SS' into datetime. Returns None if bad."""
    if isinstance(raw, bytes):
        raw = raw.decode('ascii', errors='ignore')
    raw = str(raw).strip()
    if len(raw) < 19: return None
    try:
        y = int(raw[0:4]); m = int(raw[5:7]); d = int(raw[8:10])
        hh = int(raw[11:13]); mm = int(raw[14:16]); ss = int(raw[17:19])
        return clamp_dt(datetime.datetime(y, m, d, hh, mm, ss))
    except ValueError:
        return None

def parse_db_dt(s):
    if not s: return None
    s = str(s).strip()[:19].replace('T', ' ')
    try: return clamp_dt(datetime.datetime.strptime(s, '%Y-%m-%d %H:%M:%S'))
    except ValueError: pass
    try: return clamp_dt(datetime.datetime.strptime(s[:10], '%Y-%m-%d'))
    except ValueError: return None

def gather_exif_candidates(path):
    p = Path(path)
    if p.suffix.lower() not in IMAGE_EXTS: return []
    out = []
    try:
        with Image.open(path) as img:
            exif = img.getexif()
            if not exif: return []
            sources = [exif]
            try:
                ifd = exif.get_ifd(EXIF_IFD_TAG)
                if ifd: sources.append(ifd)
            except Exception:
                pass
            for src in sources:
                for tid in EXIF_DATE_TAGS:
                    if tid in src:
                        dt = parse_exif_dt_str(src[tid])
                        if dt: out.append(dt)
            # GPS — combine GPSDateStamp ("YYYY:MM:DD") + GPSTimeStamp
            # ((h,m,s) rationals). GPS time is UTC but for ordering it's
            # close enough; we just need a comparable timestamp.
            try:
                gps = exif.get_ifd(GPS_IFD_TAG)
                if gps and GPS_DATE_TAG in gps:
                    ds = str(gps[GPS_DATE_TAG]).strip()  # "YYYY:MM:DD"
                    if len(ds) >= 10 and ds[4] == ':' and ds[7] == ':':
                        y = int(ds[0:4]); m = int(ds[5:7]); d = int(ds[8:10])
                        hh = mm = ss = 12
                        ts = gps.get(GPS_TIME_TAG)
                        if ts and len(ts) == 3:
                            try:
                                hh = int(ts[0]); mm = int(ts[1]); ss = int(ts[2])
                            except (TypeError, ValueError):
                                pass
                        dt = clamp_dt(datetime.datetime(y, m, d, hh, mm, ss))
                        if dt: out.append(dt)
            except Exception:
                pass
    except Exception:
        pass
    return out

def file_times(path):
    """Returns (mtime_dt, ctime_dt) — both may be None."""
    try:
        st = Path(path).stat()
        mt = clamp_dt(datetime.datetime.fromtimestamp(st.st_mtime))
        ct = clamp_dt(datetime.datetime.fromtimestamp(st.st_ctime))
        return (mt, ct)
    except OSError:
        return (None, None)

# Main
ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-dates-oldest-{ts}')
print(f'Backing up DB → {backup.name}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=60)
conn.row_factory = sqlite3.Row
cur = conn.cursor()
all_rows = cur.execute("SELECT id, path, date_taken FROM photos").fetchall()
total = len(all_rows)
print(f'Total photos: {total:,}')

from_exif = 0
from_mtime = 0
from_ctime = 0
unchanged = 0
file_missing = 0
no_signal = 0
batch = []
start = time.time()
for i, r in enumerate(all_rows, 1):
    if i % 500 == 0 or i == total:
        el = time.time() - start
        rate = i / el if el > 0 else 0
        eta = (total - i) / rate if rate > 0 else 0
        print(f'  {i:>6}/{total} ({100*i//total}%)  exif={from_exif} '
              f'mtime={from_mtime} ctime={from_ctime} unchanged={unchanged} '
              f'missing={file_missing} eta={int(eta)}s')
        if batch:
            cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
            conn.commit()
            batch = []
    p = r['path']
    if not Path(p).exists():
        file_missing += 1
        continue
    db_dt = parse_db_dt(r['date_taken'])

    # Candidate pool. Image files contribute EXIF; everything contributes
    # mtime + ctime.
    exif_dates = gather_exif_candidates(p)
    mtime_dt, ctime_dt = file_times(p)

    # Source preference:
    #   1) Earliest of all EXIF candidates (if any)
    #   2) Earliest of (mtime, ctime) — both are file-time hints
    canonical = None
    source = ''
    if exif_dates:
        canonical = min(exif_dates)
        source = 'exif'
    else:
        fts = [t for t in (mtime_dt, ctime_dt) if t is not None]
        if fts:
            canonical = min(fts)
            source = 'mtime' if canonical == mtime_dt else 'ctime'
    if canonical is None:
        no_signal += 1
        continue

    # Decide.
    write = False
    if db_dt is None:
        write = True
    else:
        delta = (db_dt - canonical).total_seconds()
        # Update if canonical is OLDER by > 60 s than current DB.
        # Never make the row newer.
        if delta > 60:
            write = True

    if write:
        new_str = canonical.strftime('%Y-%m-%d %H:%M:%S')
        batch.append((new_str, r['id']))
        if source == 'exif':   from_exif += 1
        elif source == 'mtime': from_mtime += 1
        elif source == 'ctime': from_ctime += 1
    else:
        unchanged += 1

if batch:
    cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
    conn.commit()

el = time.time() - start
print()
print(f'===== Done in {el:.1f}s =====')
print(f'Fixed via EXIF (oldest): {from_exif:,}')
print(f'Fixed via mtime:         {from_mtime:,}')
print(f'Fixed via ctime:         {from_ctime:,}')
print(f'Unchanged (DB OK):       {unchanged:,}')
print(f'File missing on disk:    {file_missing:,}')
print(f'No timestamp at all:     {no_signal:,}  (genuinely unreadable file)')
print()
print(f'Backup: {backup}')
conn.close()
