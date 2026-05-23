"""
v1.5.253 — Re-process photos whose date_taken sits on a known
placeholder date (Photoshop / Instagram / DOS / Unix epoch). The
in-app scanner now filters these from the candidate pool, but the
DB still has historical rows that were committed before the filter
existed. Pick a real date from path-pattern → mtime → ctime.
"""
import sqlite3, sys, io, datetime, shutil, os, re
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

PLACEHOLDER = re.compile(r'^(1970|1980|2000|2001|2002)-01-01 00:00:00$')

ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-placeholder-{ts}')
print(f'Backing up DB → {backup.name}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=60)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

# Quick approach: pick rows whose date matches the placeholder pattern.
rows = cur.execute("""
    SELECT id, path, date_taken FROM photos
    WHERE date_taken IN (
        '1970-01-01 00:00:00', '1980-01-01 00:00:00',
        '2000-01-01 00:00:00', '2001-01-01 00:00:00', '2002-01-01 00:00:00'
    )
""").fetchall()
print(f'Rows with placeholder date_taken: {len(rows)}')

# Build a candidate list per row from path pattern + mtime/ctime.
PATH_RX = re.compile(r'(?:^|[^\d])((?:19|20)\d{2})[-_./:]?(\d{2})[-_./:]?(\d{2})(?:$|[^\d])')

def path_date(p):
    m = PATH_RX.search(p)
    if not m: return None
    y, mo, d = int(m.group(1)), int(m.group(2)), int(m.group(3))
    if not (1 <= mo <= 12 and 1 <= d <= 31): return None
    try:
        return datetime.datetime(y, mo, d, 12, 0, 0)
    except ValueError:
        return None

def file_dates(p):
    try:
        st = Path(p).stat()
        return [
            datetime.datetime.fromtimestamp(st.st_mtime),
            datetime.datetime.fromtimestamp(st.st_ctime),
        ]
    except OSError:
        return []

now = datetime.datetime.now()
floor = datetime.datetime(1990, 1, 1)

fixed_path = 0
fixed_file = 0
unchanged = 0
batch = []
for r in rows:
    p = r['path']
    cands = []
    pd = path_date(p)
    if pd and floor <= pd <= now: cands.append(('path', pd))
    for d in file_dates(p):
        if floor <= d <= now: cands.append(('file', d))
    if not cands:
        unchanged += 1
        continue
    # Oldest wins; among ties prefer 'file' source (real wall-clock) over 'path' (synthesized noon).
    cands.sort(key=lambda x: (x[1], 0 if x[0]=='file' else 1))
    src, chosen = cands[0]
    new = chosen.strftime('%Y-%m-%d %H:%M:%S')
    batch.append((new, r['id']))
    if src == 'path': fixed_path += 1
    else: fixed_file += 1
    print(f'  id={r["id"]} {r["date_taken"]} → {new} ({src})  {Path(p).name}')

if batch:
    cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
    conn.commit()

print()
print(f'Fixed via path pattern: {fixed_path}')
print(f'Fixed via mtime/ctime:  {fixed_file}')
print(f'No signal (unchanged):  {unchanged}')
print(f'Backup: {backup}')
conn.close()
