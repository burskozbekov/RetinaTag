"""
v1.5.229 — Look at face_regions for the photo currently shown
(filename 5.jpg, 2738x1825). Are coords pixel-space or normalized?
"""
import sqlite3, sys, io
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
conn = sqlite3.connect(DB, timeout=10)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

# Find photos with filename '5.jpg' that are 2738x1825
rows = cur.execute(
    """SELECT id, path, width, height
         FROM photos
        WHERE filename = '5.jpg' AND width = 2738 AND height = 1825"""
).fetchall()
print(f'Candidates: {len(rows)}')
for r in rows:
    print(f'  id={r["id"]:>6} path={r["path"]}')

for r in rows:
    print()
    print(f'=== Photo id={r["id"]} ({r["path"].split(chr(92))[-1]}) {r["width"]}x{r["height"]} ===')
    faces = cur.execute(
        """SELECT fr.id, fr.x1, fr.y1, fr.x2, fr.y2, fr.score, p.name AS person_name
             FROM face_regions fr
             LEFT JOIN persons p ON p.id = fr.person_id
            WHERE fr.photo_id = ?
            ORDER BY fr.x1""",
        (r['id'],)
    ).fetchall()
    print(f'  {len(faces)} face_regions')
    for f in faces:
        w = f['x2'] - f['x1']
        h = f['y2'] - f['y1']
        pct_x1 = 100 * f['x1'] / r['width']
        pct_y1 = 100 * f['y1'] / r['height']
        pct_w  = 100 * w / r['width']
        pct_h  = 100 * h / r['height']
        n = f['person_name'] or '-'
        print(f'  face_id={f["id"]:>5} x1={f["x1"]:>5} y1={f["y1"]:>5} '
              f'w={w:>4} h={h:>4}  ({pct_x1:5.1f}% {pct_y1:5.1f}%  {pct_w:.1f}%×{pct_h:.1f}%)  {n}')

conn.close()
