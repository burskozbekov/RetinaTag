"""
v1.5.252 — Photos whose date_taken time-of-day is exactly noon-ish
(00:00:00, 12:00:00, 12:12:12) almost certainly got their *date* from
a folder-name path pattern (D:\Fotograflar\DUZENLE\2020_08_08\) with a
synthesized fallback time. The real EXIF DateTimeOriginal often has
the *correct* time on the *same date* — we just never used it because
fix_dates_oldest.py preferred the synthesized noon over the real time.

This script:
  - Picks rows where the time component looks synthesized.
  - Re-reads EXIF DateTimeOriginal / DateTimeDigitized / DateTime.
  - If any EXIF tag has the SAME date but a real time, use it.
  - Else if mtime/ctime falls on that date, use that time.
  - Otherwise leave the row alone (date is from folder pattern, no
    better time signal exists).

Run with the app closed.
"""
import sqlite3, sys, io, datetime, shutil, time, os
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

# Times we treat as synthesized fallbacks.
SUSPECT_TIMES = ('00:00:00', '12:00:00', '12:12:12')

def parse_exif_dt_str(raw):
    if isinstance(raw, bytes):
        raw = raw.decode('ascii', errors='ignore')
    raw = str(raw).strip()
    if len(raw) < 19: return None
    try:
        y = int(raw[0:4]); m = int(raw[5:7]); d = int(raw[8:10])
        hh = int(raw[11:13]); mm = int(raw[14:16]); ss = int(raw[17:19])
        return datetime.datetime(y, m, d, hh, mm, ss)
    except ValueError:
        return None

def gather_exif_dts(path):
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
    except Exception:
        pass
    return out

def file_times(path):
    try:
        st = Path(path).stat()
        return (
            datetime.datetime.fromtimestamp(st.st_mtime),
            datetime.datetime.fromtimestamp(st.st_ctime),
        )
    except OSError:
        return (None, None)

# Build the suspect set in SQL — fast.
ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-synth-times-{ts}')
print(f'Backing up DB → {backup.name}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=60)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

suspect_clause = ' OR '.join(f"date_taken LIKE '% {t}'" for t in SUSPECT_TIMES)
rows = cur.execute(f"SELECT id, path, date_taken FROM photos WHERE {suspect_clause}").fetchall()
total = len(rows)
print(f'Suspect rows (synthesized noon/midnight times): {total:,}')

fixed_exif = 0
fixed_mtime = 0
unchanged_no_signal = 0
missing = 0
batch = []
start = time.time()

for i, r in enumerate(rows, 1):
    if i % 200 == 0 or i == total:
        el = time.time() - start
        rate = i/el if el>0 else 0
        eta = (total-i)/rate if rate>0 else 0
        print(f'  {i:>5}/{total}  exif={fixed_exif} mtime={fixed_mtime} none={unchanged_no_signal} missing={missing} eta={int(eta)}s')
        if batch:
            cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
            conn.commit()
            batch = []
    p = r['path']
    if not Path(p).exists():
        missing += 1
        continue
    # The current DB date — we ONLY want to keep this date, just fix the time.
    cur_str = r['date_taken'][:10]
    try:
        cur_date = datetime.datetime.strptime(cur_str, '%Y-%m-%d').date()
    except ValueError:
        unchanged_no_signal += 1
        continue

    # Look for an EXIF timestamp on the SAME date with a non-noon time.
    best_dt = None
    for dt in gather_exif_dts(p):
        if dt.date() == cur_date:
            # Got a real EXIF time on the same date — use the earliest if multiple.
            if best_dt is None or dt < best_dt:
                best_dt = dt
    if best_dt is not None:
        new_str = best_dt.strftime('%Y-%m-%d %H:%M:%S')
        if new_str != r['date_taken']:
            batch.append((new_str, r['id']))
            fixed_exif += 1
        else:
            unchanged_no_signal += 1
        continue

    # Try mtime / ctime on the same date.
    mt, ct = file_times(p)
    candidates = [t for t in (mt, ct) if t and t.date() == cur_date]
    if candidates:
        best = min(candidates)
        new_str = best.strftime('%Y-%m-%d %H:%M:%S')
        if new_str != r['date_taken']:
            batch.append((new_str, r['id']))
            fixed_mtime += 1
        else:
            unchanged_no_signal += 1
        continue

    # No signal on that date — leave the row alone (date probably came from
    # the folder name; we have no better time information).
    unchanged_no_signal += 1

if batch:
    cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
    conn.commit()

el = time.time() - start
print()
print(f'===== Done in {el:.1f}s =====')
print(f'Fixed via EXIF time: {fixed_exif:,}')
print(f'Fixed via mtime:     {fixed_mtime:,}')
print(f'No same-date signal: {unchanged_no_signal:,} (kept as-is)')
print(f'File missing:        {missing:,}')
print()
print(f'Backup: {backup}')
conn.close()
