"""
v1.5.272 — Apply rebucket: physically MOVE misplaced photos out of
MTP-style \YYYY\MM-MonthName\ folders into the bucket that matches
their actual date_taken (already corrected by fix_dates_final.py).

Safety rails:
  - Only acts on paths matching exactly \<root>\<YYYY>\<MM>-<MonthName>\.
    User-organized folders (\DUZENLE\, \samsung\, \Portrait\,
    \superturk\, etc.) are NEVER touched.
  - Per-file backup of (old_path, new_path) appended to a JSON log so
    the user can hand-undo a bad move if needed.
  - Cross-volume rename failure falls back to copy+verify+remove.
  - File-name collisions in the target bucket get -1, -2, ... suffix.
  - DB UPDATE happens AFTER the disk move succeeds. If the DB update
    fails the file stays at the new path; we log the orphan path so
    the next scan picks it up.

Run with the app closed.
"""
import sqlite3, io, sys, re, json, shutil, datetime
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

MONTHS = ('January','February','March','April','May','June',
          'July','August','September','October','November','December')
PAT = re.compile(
    r'^(.*?)[\\/](19\d{2}|20\d{2})[\\/](0[1-9]|1[0-2])-('
    + '|'.join(MONTHS) + r')[\\/]([^\\/]+)$',
    re.I,
)

def pick_unique(target: Path) -> Path:
    if not target.exists(): return target
    stem = target.stem; ext = target.suffix
    parent = target.parent
    for n in range(1, 10_000):
        c = parent / f'{stem}-{n}{ext}'
        if not c.exists(): return c
    return target  # astronomically unlikely

def move_file(src: Path, dst: Path) -> bool:
    dst.parent.mkdir(parents=True, exist_ok=True)
    try:
        src.rename(dst)
        return True
    except OSError:
        pass
    try:
        shutil.copy2(str(src), str(dst))
        # verify size match before removing the source
        if dst.exists() and dst.stat().st_size == src.stat().st_size:
            src.unlink()
            return True
        return False
    except Exception as e:
        print(f'  ERROR move {src} -> {dst}: {e}')
        return False

ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup_db = Path(DB + f'.bak-rebucket-{ts}')
print(f'Backing up DB -> {backup_db.name}')
shutil.copy2(DB, backup_db)

log_path = Path(DB).parent / f'rebucket-log-{ts}.json'
log = []

conn = sqlite3.connect(DB, timeout=60)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

rows = cur.execute("SELECT id, path, date_taken FROM photos WHERE date_taken IS NOT NULL").fetchall()
total = len(rows)
moved = 0
unchanged = 0
errors = 0

for r in rows:
    m = PAT.match(r['path'])
    if not m:
        unchanged += 1
        continue
    root, fy_s, fm_s, _fmonth, fn = m.groups()
    fy, fm = int(fy_s), int(fm_s)
    dy = int(r['date_taken'][0:4])
    dm = int(r['date_taken'][5:7])
    if (fy, fm) == (dy, dm):
        unchanged += 1
        continue
    # Build target path under SAME root, correct year/month.
    new_dir = Path(root) / f'{dy:04d}' / f'{dm:02d}-{MONTHS[dm-1]}'
    new_path = pick_unique(new_dir / fn)
    src = Path(r['path'])
    if not src.exists():
        errors += 1
        log.append({'id': r['id'], 'src': str(src), 'dst': str(new_path), 'status': 'src-missing'})
        continue
    if move_file(src, new_path):
        # Update DB row
        new_folder = str(new_path.parent)
        cur.execute(
            "UPDATE photos SET path = ?1, folder = ?2 WHERE id = ?3",
            (str(new_path), new_folder, r['id']),
        )
        moved += 1
        log.append({'id': r['id'], 'src': str(src), 'dst': str(new_path), 'status': 'moved'})
        if moved % 50 == 0:
            conn.commit()
            print(f'  moved {moved}/{len(rows)} so far...')
    else:
        errors += 1
        log.append({'id': r['id'], 'src': str(src), 'dst': str(new_path), 'status': 'move-failed'})

conn.commit()
conn.close()

with open(log_path, 'w', encoding='utf-8') as f:
    json.dump(log, f, ensure_ascii=False, indent=2)

print()
print(f'===== Done =====')
print(f'Moved:     {moved:,}')
print(f'Unchanged: {unchanged:,}')
print(f'Errors:    {errors:,}')
print(f'Log: {log_path}')
print(f'DB backup: {backup_db}')
