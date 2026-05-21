"""
v1.5.224 — Diagnostic: find files with suspicious date_taken / size.

Reports rows where:
  1. date_taken is the import day (likely mtime-fallback, not real EXIF)
  2. media_type='image' but file size > 25 MB (probably a misclassified
     video or a corrupted file)
  3. file is huge AND date_taken matches today (the screenshot case)
  4. file path on disk doesn't exist

Read-only — no writes. Run with RetinaTag open or closed.
"""
import sqlite3, os, datetime
from pathlib import Path

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
today_iso = datetime.date.today().isoformat()

conn = sqlite3.connect(DB, timeout=10)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

print(f'Today: {today_iso}')
print()

# 1. How many photos have date_taken on today's date?
n_today = cur.execute(
    "SELECT COUNT(*) FROM photos WHERE date_taken LIKE ?",
    (today_iso + '%',)
).fetchone()[0]
print(f'Photos with date_taken on today ({today_iso}): {n_today}')

# 2. Big-file images
print()
print('Images larger than 25 MB (suspicious — likely videos or corrupt):')
big = cur.execute(
    """SELECT id, path, ROUND(CAST(size AS REAL)/1048576, 1) AS mb,
              media_type, date_taken, status
         FROM photos
        WHERE size > 25 * 1048576
          AND media_type = 'image'
        ORDER BY size DESC
        LIMIT 20"""
).fetchall()
for r in big:
    fn = Path(r['path']).name
    exists = 'OK' if Path(r['path']).exists() else 'MISSING'
    print(f"  id={r['id']:>6}  {r['mb']:>7.1f} MB  type={r['media_type']:<5} dt={r['date_taken'] or '-':<20} {exists}  {fn}")

# 3. Today-dated + huge
print()
print('Today-dated AND > 50 MB:')
today_big = cur.execute(
    """SELECT id, path, ROUND(CAST(size AS REAL)/1048576, 1) AS mb,
              media_type, date_taken
         FROM photos
        WHERE date_taken LIKE ? AND size > 50 * 1048576
        ORDER BY size DESC
        LIMIT 20""",
    (today_iso + '%',)
).fetchall()
for r in today_big:
    fn = Path(r['path']).name
    print(f"  id={r['id']:>6}  {r['mb']:>7.1f} MB  type={r['media_type']:<5} dt={r['date_taken']}  {fn}")

# 4. Recently-added photos with date_taken == today's pattern (the symptom)
print()
print('Sample of today-dated photos (the symptom — likely orphans rescued from Unknown):')
samples = cur.execute(
    """SELECT id, path, ROUND(CAST(size AS REAL)/1048576, 1) AS mb,
              media_type, date_taken, status
         FROM photos
        WHERE date_taken LIKE ?
        ORDER BY size DESC
        LIMIT 10""",
    (today_iso + '%',)
).fetchall()
for r in samples:
    fn = Path(r['path']).name
    exists = 'OK' if Path(r['path']).exists() else 'MISSING'
    print(f"  id={r['id']:>6}  {r['mb']:>7.1f} MB  type={r['media_type']:<5} dt={r['date_taken']:<20} {exists}  {fn}")

# 5. Files where path doesn't exist on disk
print()
print('DB rows pointing at non-existent files (orphan DB rows):')
all_paths = cur.execute("SELECT id, path, ROUND(CAST(size AS REAL)/1048576, 1) AS mb FROM photos").fetchall()
missing = []
for r in all_paths:
    if not Path(r['path']).exists():
        missing.append(r)
print(f'  Total orphan DB rows: {len(missing)}')
for r in missing[:10]:
    print(f"  id={r['id']:>6}  {r['mb']:>7.1f} MB  {r['path']}")

conn.close()
