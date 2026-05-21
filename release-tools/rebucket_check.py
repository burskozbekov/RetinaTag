import sqlite3, os
from pathlib import Path

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
UNKNOWN = Path(r'D:\Fotograflar\Unknown\Unknown')

conn = sqlite3.connect(DB, timeout=30)
cur = conn.cursor()

n_unknown = cur.execute(
    "SELECT COUNT(*) FROM photos WHERE path LIKE '%\\Unknown\\Unknown\\%'"
).fetchone()[0]

phys = list(UNKNOWN.iterdir()) if UNKNOWN.exists() else []
print(f'DB rows still pointing at Unknown/Unknown: {n_unknown}')
print(f'Physical files still in Unknown/Unknown:   {len(phys)}')
print()
print('Files in Unknown that DB does NOT know about (orphans on disk):')
db_paths_in_unknown = {r[0] for r in cur.execute(
    "SELECT path FROM photos WHERE path LIKE '%\\Unknown\\Unknown\\%'"
)}
for f in phys[:5]:
    in_db = str(f) in db_paths_in_unknown
    print(f'  {f.name:25s} {"IN DB" if in_db else "ORPHAN"}')

print()
print('Sample DB rows pointing at Unknown:')
for path in list(db_paths_in_unknown)[:5]:
    exists = Path(path).exists()
    print(f'  {Path(path).name:25s} {"FILE STILL THERE" if exists else "FILE MISSING"}')

# Count: DB says Unknown, file still there = need-to-process
# DB says Unknown, file missing = inconsistent state (need to find new location)
still_there = sum(1 for p in db_paths_in_unknown if Path(p).exists())
missing = len(db_paths_in_unknown) - still_there
print()
print(f'Summary:')
print(f'  DB rows still in Unknown, file still there:  {still_there}')
print(f'  DB rows still in Unknown, file MISSING:      {missing}')
print(f'  (the {missing} need DB-side path repair via filename search)')

conn.close()
