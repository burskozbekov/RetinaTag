import sqlite3, sys, io
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')
DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
conn = sqlite3.connect(DB, timeout=10)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

# Two populated entries — see format
print('=== Persons with thumbnail populated ===')
rows = cur.execute(
    "SELECT id, name, thumbnail FROM persons WHERE thumbnail IS NOT NULL AND thumbnail != '' LIMIT 5"
).fetchall()
for r in rows:
    t = r['thumbnail']
    snippet = (t[:160] + '…') if len(t) > 160 else t
    print(f"  id={r['id']:<4} name={r['name']:<20} thumbnail={snippet!r}")
    print(f"           full length: {len(t)} chars")

# Count faces by person — which persons have lots of faces (good thumb candidates)?
print()
print('=== Top 10 persons by face count ===')
for r in cur.execute("""
    SELECT p.id, p.name, COUNT(fr.id) AS n
      FROM persons p
      LEFT JOIN face_regions fr ON fr.person_id = p.id
     GROUP BY p.id
     ORDER BY n DESC
     LIMIT 10
"""):
    print(f"  id={r['id']:<4} {r['name']:<20} {r['n']} faces")

conn.close()
