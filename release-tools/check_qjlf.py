import sqlite3
from pathlib import Path

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
conn = sqlite3.connect(DB, timeout=30)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

for name in ('QJLF5557.MP4', 'IMG_4097.MOV', 'KVJN4974.JPEG'):
    rows = cur.execute(
        "SELECT id, path, size, media_type, date_taken FROM photos WHERE filename = ? OR path LIKE ?",
        (name, '%' + name)
    ).fetchall()
    print(f'\n=== {name} ===')
    for r in rows:
        exists = Path(r['path']).exists()
        print(f"  id={r['id']:>6} size={r['size']:>10} type={r['media_type']:<5} dt={r['date_taken']}")
        print(f"         path={r['path']}")
        print(f"         exists={exists}")
