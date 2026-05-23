"""
Diagnostic: find photos with suspect date_taken values.
- 12:00:00 (noon) — likely a synthesized fallback
- 00:00:00 (midnight) — likely date-only EXIF / mtime stripped
- Check which path produces these
"""
import sqlite3, sys, io, os
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
conn = sqlite3.connect(DB, timeout=30)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

# Total photos
total = cur.execute("SELECT COUNT(*) FROM photos").fetchone()[0]
print(f'Total photos: {total}')

# By time-of-day pattern
patterns = [
    ('12:00:00', "date_taken LIKE '% 12:00:00'"),
    ('00:00:00', "date_taken LIKE '% 00:00:00'"),
    ('Other'   , "date_taken NOT LIKE '% 12:00:00' AND date_taken NOT LIKE '% 00:00:00'"),
    ('NULL'    , "date_taken IS NULL"),
]
print('\nTime-of-day distribution:')
for name, where in patterns:
    n = cur.execute(f"SELECT COUNT(*) FROM photos WHERE {where}").fetchone()[0]
    pct = 100*n/total if total else 0
    print(f'  {name:>10}: {n:>6} ({pct:5.1f}%)')

# Samples of each
print('\nSample 12:00:00 dates:')
for r in cur.execute("SELECT id, filename, date_taken, path FROM photos WHERE date_taken LIKE '% 12:00:00' ORDER BY RANDOM() LIMIT 10").fetchall():
    print(f'  id={r["id"]:>5} {r["date_taken"]} {r["filename"][:30]:<30} → {r["path"][:60]}')

print('\nSample 00:00:00 dates:')
for r in cur.execute("SELECT id, filename, date_taken, path FROM photos WHERE date_taken LIKE '% 00:00:00' ORDER BY RANDOM() LIMIT 10").fetchall():
    print(f'  id={r["id"]:>5} {r["date_taken"]} {r["filename"][:30]:<30} → {r["path"][:60]}')

# Most-common date_taken values (clustering = suspicious; means many photos
# share the same exact timestamp, e.g. all stamped 2021-02-24 12:00:00).
print('\nTop 15 most-common date_taken values:')
for r in cur.execute("""
    SELECT date_taken, COUNT(*) c
    FROM photos
    WHERE date_taken IS NOT NULL
    GROUP BY date_taken
    ORDER BY c DESC
    LIMIT 15
""").fetchall():
    print(f'  {r["date_taken"]}: {r["c"]} photos')

# What does scanner write for newly-imported photos?
# Show 10 most-recently-added rows.
print('\nLast 10 photos by added timestamp (or rowid):')
try:
    last = cur.execute("SELECT id, filename, date_taken, added_at, path FROM photos ORDER BY id DESC LIMIT 10").fetchall()
except sqlite3.OperationalError:
    last = cur.execute("SELECT id, filename, date_taken, path FROM photos ORDER BY id DESC LIMIT 10").fetchall()
for r in last:
    added = r['added_at'] if 'added_at' in r.keys() else ''
    print(f'  id={r["id"]:>5} dt={r["date_taken"]} added={added} {r["filename"][:30]}')

conn.close()
