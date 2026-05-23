"""Forensic inspection of "blank video tile" cases."""
import sqlite3, sys, io, subprocess
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
conn = sqlite3.connect(DB, timeout=10)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

names = ['IMG_3611.MOV', 'IMG_E3615.MOV', 'IMG_5412.MOV', 'KWRK6815.MOV',
         'JMFA1102.MOV', 'IMG_5913.MOV', 'IMG_5917.MOV', 'UUHZ2717.MP4']
for name in names:
    rows = cur.execute(
        "SELECT id, path, size, hash, media_type, thumbnail_path, date_taken "
        "FROM photos WHERE filename = ?", (name,)
    ).fetchall()
    print(f'\n=== {name} — {len(rows)} row(s) ===')
    for r in rows:
        p = Path(r['path'])
        file_ok = p.exists()
        disk_size = p.stat().st_size if file_ok else 0
        thumb_path = r['thumbnail_path']
        thumb_ok = Path(thumb_path).exists() if thumb_path else False
        thumb_size = Path(thumb_path).stat().st_size if (thumb_path and thumb_ok) else 0
        print(f'  id={r["id"]:>5} db_size={r["size"]:>11}  disk_size={disk_size:>11}  '
              f'file_ok={file_ok}  thumb_ok={thumb_ok}  thumb_bytes={thumb_size}  date={r["date_taken"]}')
        print(f'         path={r["path"]}')
        print(f'         thumb={thumb_path}')

# Broader stats: what proportion of videos have a usable thumb file right now?
print('\n=== Library-wide video thumb coverage ===')
vid_rows = cur.execute(
    "SELECT id, path, size, thumbnail_path FROM photos WHERE media_type='video'"
).fetchall()
buckets = {'all_ok': 0, 'file_missing': 0, 'thumb_null': 0,
           'thumb_missing': 0, 'thumb_zero': 0, 'zero_size_file': 0}
for r in vid_rows:
    p = Path(r['path'])
    if not p.exists():
        buckets['file_missing'] += 1
        continue
    if p.stat().st_size == 0:
        buckets['zero_size_file'] += 1
        continue
    if not r['thumbnail_path']:
        buckets['thumb_null'] += 1
        continue
    tp = Path(r['thumbnail_path'])
    if not tp.exists():
        buckets['thumb_missing'] += 1
        continue
    if tp.stat().st_size == 0:
        buckets['thumb_zero'] += 1
        continue
    buckets['all_ok'] += 1
print(f"Total videos: {len(vid_rows):,}")
for k, v in buckets.items():
    print(f"  {k}: {v:,}")
conn.close()
