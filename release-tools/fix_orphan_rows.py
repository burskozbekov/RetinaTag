"""
v1.5.224 — Patch up the 3 orphan DB rows the user found.

Symptom: DB row points at /2025/10-October/... but the real file
lives at /2026/05-May/... The v1 rebucket script aborted on UNIQUE
constraint, v2's filename-lookup repair picked stale fn_index
entries because two same-named files existed in different buckets.

Fix: for every DB row whose path doesn't exist on disk, do a
filename + (preferably) size search across the library. If a
unique match exists with the same size, patch the row in-place.

Run with RetinaTag CLOSED.
"""
import sqlite3, os, datetime, shutil
from pathlib import Path

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
ROOT = Path(r'D:\Fotograflar')

# Back up DB first
ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-orphan-{ts}')
print(f'Backup: {DB} -> {backup}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=30)
cur = conn.cursor()

# Find every DB row whose path doesn't exist
rows = cur.execute("SELECT id, path, size FROM photos").fetchall()
orphans = [(rid, path, sz) for (rid, path, sz) in rows if not Path(path).exists()]
print(f'Orphan DB rows: {len(orphans)}')

# Index files by (name, size) across all YYYY/MM-Month/ buckets
print('Indexing library by (name, size) …')
fs_index = {}  # (filename, size) -> Path
for entry in ROOT.rglob('*'):
    if not entry.is_file():
        continue
    try:
        sz = entry.stat().st_size
    except OSError:
        continue
    fs_index[(entry.name, sz)] = entry
print(f'  {len(fs_index):,} files indexed')

fixed = 0
deleted = 0
unmatched = []
for rid, old_path, sz in orphans:
    name = Path(old_path).name
    target = fs_index.get((name, sz))
    if target is None:
        unmatched.append((rid, old_path, sz))
        continue
    new_folder = str(target.parent)
    new_path = str(target)
    try:
        cur.execute(
            'UPDATE photos SET path = ?, folder = ? WHERE id = ?',
            (new_path, new_folder, rid)
        )
        fixed += 1
        print(f'  fixed id={rid}  {name}  -> {new_path}')
    except sqlite3.IntegrityError:
        # Another row already owns the destination path — drop the orphan
        cur.execute('DELETE FROM photos WHERE id = ?', (rid,))
        deleted += 1
        print(f'  dup-dropped id={rid}  {name}')

print()
print(f'Unmatched (no file with same name+size found):')
for rid, p, sz in unmatched:
    print(f'  id={rid:>6} size={sz:>10}  {p}')

conn.commit()
conn.close()

print()
print(f'Summary: fixed={fixed}, dup-dropped={deleted}, unmatched={len(unmatched)}')
print(f'Backup: {backup}')
