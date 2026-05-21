"""
v1.5.226 — Are person thumbnails on disk / in DB on PC?
"""
import sqlite3
from pathlib import Path

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
conn = sqlite3.connect(DB, timeout=10)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

# What columns does the persons table have?
print('=== persons schema ===')
for row in cur.execute("PRAGMA table_info(persons)"):
    print(f'  {row["name"]:<30} {row["type"]}')

# Sample of people the user sees in screenshot
print()
print('=== Sample people from screenshot ===')
for name in ('Ali Can Bombadil', 'Arzu Balkan', 'Asuman', 'Ati', 'Babaanne'):
    rows = cur.execute(
        "SELECT * FROM persons WHERE name = ?",
        (name,)
    ).fetchall()
    for r in rows:
        d = dict(r)
        # Cut down big blobs
        for k in list(d.keys()):
            v = d[k]
            if isinstance(v, bytes):
                d[k] = f'<bytes len={len(v)}>'
            elif isinstance(v, str) and len(v) > 80:
                d[k] = v[:80] + '…'
        print(f'  {d}')

# Count: people with thumb data vs without
print()
print('=== Thumb coverage ===')
cols = [r['name'] for r in cur.execute("PRAGMA table_info(persons)")]
thumb_cols = [c for c in cols if 'thumb' in c.lower() or 'avatar' in c.lower() or 'icon' in c.lower()]
print(f'Candidate thumb columns: {thumb_cols}')
for col in thumb_cols:
    have = cur.execute(f'SELECT COUNT(*) FROM persons WHERE {col} IS NOT NULL AND {col} != ""').fetchone()[0]
    total = cur.execute('SELECT COUNT(*) FROM persons').fetchone()[0]
    print(f'  {col}: {have} / {total} populated')

# Also check faces table for thumb-like columns
print()
print('=== faces / face_regions schemas ===')
for t in ('faces', 'face_regions'):
    rows = list(cur.execute(f'PRAGMA table_info({t})'))
    if rows:
        print(f'  {t}:')
        for r in rows:
            print(f'    {r["name"]:<30} {r["type"]}')

conn.close()
