"""
v1.5.263 — Library-wide date repair, comprehensive.

User: "Bu fotoğraf kesinlikle 2026'da değil ya!!! Şunu niye komple
düzeltemiyorsun!!" — a 1941 Turkish coin photo got date_taken =
2026-05-22 (file mtime when copied to PC). The folder is
D:\Fotograflar\2016-10\IQBQ7822.jpg, EXIF is empty.

Earlier passes (fix_dates_oldest.py + fix_synthesized_times.py +
fix_placeholder_dates.py) handled three specific patterns. This
pass walks EVERY row and applies the canonical, current scanner
rules:

  1. Collect every candidate:
     - EXIF DateTimeOriginal / Digitized / DateTime (filter known
       Photoshop/DOS/Unix placeholders 1970/1980/2000/2001/2002
       at exactly Jan 1 midnight)
     - File mtime, ctime
     - Path-pattern YYYY-MM-DD (synth noon, lowest quality)
     - Path-pattern YYYY-MM (synth day=1 noon, lowest quality)
  2. Clamp 1990-01-01 ≤ d ≤ now+1day
  3. Pick the OLDEST DATE across all surviving candidates.
  4. Among candidates on that date, pick the HIGHEST quality time
     (EXIF=3 > mtime=2 > GPS=1 > path=0).
  5. If the chosen value differs from the DB row, write it.

This converges every row to what `scanner::extract_date_taken`
would emit today for a fresh import — fixing photos that were
imported under older buggy logic.

Run with the app closed.
"""
import sqlite3, sys, io, datetime, shutil, time, re, os
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

try:
    from PIL import Image
except ImportError:
    print('Pillow required: pip install Pillow'); sys.exit(1)

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

EXIF_DATE_TAGS = (0x9003, 0x9004, 0x0132)
EXIF_IFD_TAG   = 0x8769
IMAGE_EXTS = {'.jpg', '.jpeg', '.png', '.tif', '.tiff', '.heic', '.heif', '.webp'}

YEAR_MIN, YEAR_MAX = 1990, 2099
NOW = datetime.datetime.now() + datetime.timedelta(days=1)
FLOOR = datetime.datetime(1990, 1, 1)

# Known stub combos to drop from EXIF.
def is_placeholder_exif(dt):
    if dt.hour != 0 or dt.minute != 0 or dt.second != 0: return False
    if dt.month != 1 or dt.day != 1: return False
    return dt.year in (1970, 1980, 2000, 2001, 2002)

def parse_exif_dt_str(raw):
    if isinstance(raw, bytes): raw = raw.decode('ascii', errors='ignore')
    raw = str(raw).strip()
    if len(raw) < 19: return None
    try:
        y = int(raw[0:4]); m = int(raw[5:7]); d = int(raw[8:10])
        hh = int(raw[11:13]); mm = int(raw[14:16]); ss = int(raw[17:19])
        return datetime.datetime(y, m, d, hh, mm, ss)
    except ValueError:
        return None

def gather_exif(path):
    """Return list of (dt, quality=3) from EXIF date tags. Placeholders dropped."""
    out = []
    p = Path(path)
    if p.suffix.lower() not in IMAGE_EXTS: return out
    try:
        with Image.open(path) as img:
            exif = img.getexif()
            if not exif: return out
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
                        if dt and not is_placeholder_exif(dt):
                            out.append((dt, 3))
    except Exception:
        pass
    return out

# Path scanners (YYYY-MM-DD and YYYY-MM) — match the Rust scanner.
RX_FULL = re.compile(r'(?:^|[^\d])((?:19|20)\d{2})[-_./:]?(\d{2})[-_./:]?(\d{2})(?:$|[^\d])')
RX_YM   = re.compile(r'(?:^|[^\d])((?:19|20)\d{2})[-_./:]?(\d{2})(?:$|[^\d])')

def gather_path(path):
    """Return list of (dt, q=0) from YYYY-MM-DD and YYYY-MM matches in the path."""
    out = []
    seen = set()
    for m in RX_FULL.finditer(path):
        y, mo, d = int(m.group(1)), int(m.group(2)), int(m.group(3))
        if not (YEAR_MIN <= y <= YEAR_MAX): continue
        if not (1 <= mo <= 12): continue
        if not (1 <= d <= 31): continue
        try:
            dt = datetime.datetime(y, mo, d, 12, 0, 0)
            if (y, mo, d) not in seen:
                seen.add((y, mo, d))
                out.append((dt, 0))
        except ValueError:
            pass
    # YYYY-MM (without a day) — only contribute if no full match for the same y-m.
    for m in RX_YM.finditer(path):
        y, mo = int(m.group(1)), int(m.group(2))
        if not (YEAR_MIN <= y <= YEAR_MAX): continue
        if not (1 <= mo <= 12): continue
        # Skip if a full YYYY-MM-DD for this y-m was already captured.
        if any(s[0]==y and s[1]==mo for s in seen): continue
        try:
            dt = datetime.datetime(y, mo, 1, 12, 0, 0)
            out.append((dt, 0))
        except ValueError:
            pass
    return out

def gather_fs(path):
    """Return [(mtime, 2), (ctime, 2)] if available."""
    out = []
    try:
        st = Path(path).stat()
        out.append((datetime.datetime.fromtimestamp(st.st_mtime), 2))
        out.append((datetime.datetime.fromtimestamp(st.st_ctime), 2))
    except OSError:
        pass
    return out

def best_date_for(path):
    """Apply current scanner rules and return the canonical YYYY-MM-DD HH:MM:SS string."""
    cands = []
    cands += gather_exif(path)
    cands += gather_fs(path)
    cands += gather_path(path)
    # Clamp plausible window.
    cands = [(dt, q) for dt, q in cands if FLOOR <= dt <= NOW]
    if not cands: return None
    # Oldest DATE.
    oldest_date = min(dt.date() for dt, _ in cands)
    # Among that date, highest quality time. Ties → earliest time.
    candsOnDate = [(dt, q) for dt, q in cands if dt.date() == oldest_date]
    candsOnDate.sort(key=lambda t: (-t[1], t[0]))
    chosen = candsOnDate[0][0]
    return chosen.strftime('%Y-%m-%d %H:%M:%S')

ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-comp-dates-{ts}')
print(f'Backing up DB → {backup.name}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=60)
conn.row_factory = sqlite3.Row
cur = conn.cursor()
rows = cur.execute("SELECT id, path, date_taken FROM photos").fetchall()
total = len(rows)
print(f'Total photos: {total:,}')

updated = 0
unchanged = 0
missing = 0
batch = []
start = time.time()

for i, r in enumerate(rows, 1):
    if i % 1000 == 0 or i == total:
        el = time.time() - start
        rate = i/el if el>0 else 0
        eta = (total-i)/rate if rate>0 else 0
        print(f'  {i:>6}/{total} updated={updated} unchanged={unchanged} missing={missing} eta={int(eta)}s')
        if batch:
            cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
            conn.commit()
            batch = []
    p = r['path']
    if not Path(p).exists():
        missing += 1
        continue
    canonical = best_date_for(p)
    if not canonical:
        unchanged += 1
        continue
    if canonical == r['date_taken']:
        unchanged += 1
        continue
    batch.append((canonical, r['id']))
    updated += 1

if batch:
    cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
    conn.commit()

el = time.time() - start
print()
print(f'===== Done in {el:.1f}s =====')
print(f'Updated:   {updated:,}')
print(f'Unchanged: {unchanged:,}')
print(f'Missing:   {missing:,}')
print(f'Backup: {backup}')
conn.close()
