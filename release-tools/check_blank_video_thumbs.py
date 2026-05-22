"""Check why specific videos have no thumbnail."""
import sqlite3, sys, io, subprocess
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
THUMBS_DIR = Path(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\thumbnails')

conn = sqlite3.connect(DB, timeout=10)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

names = ['IMG_5935.MOV', 'IMG_5233.MOV', 'VYPCE8346.MOV', 'IMG_3616.MOV',
         'IMG_5420.MOV', 'CGNT6692.MOV', 'IMG_5575.MOV', 'IMG_5909.MOV']
print('=== Files from screenshot ===')
for name in names:
    rows = cur.execute(
        "SELECT id, path, hash, media_type, thumbnail_path, size FROM photos WHERE filename = ?",
        (name,)
    ).fetchall()
    for r in rows:
        file_ok = Path(r['path']).exists()
        thumb_in_db = r['thumbnail_path']
        thumb_file_ok = Path(thumb_in_db).exists() if thumb_in_db else False
        mb = round(r['size'] / 1048576, 1) if r['size'] else 0
        print(f'\n  id={r["id"]:>5}  {name}  ({mb} MB)')
        print(f'    path={r["path"]}  file_exists={file_ok}')
        print(f'    thumbnail_path={thumb_in_db}')
        print(f'    thumb file exists: {thumb_file_ok}')

# Check ffmpeg availability — thumbnail generation needs it
print('\n=== ffmpeg check ===')
try:
    r = subprocess.run(['ffmpeg', '-version'], capture_output=True, text=True, timeout=5)
    print(f'  ffmpeg returncode={r.returncode}')
    if r.stdout:
        print(f'  ffmpeg version: {r.stdout.splitlines()[0]}')
except FileNotFoundError:
    print('  ffmpeg NOT FOUND on PATH')
except Exception as e:
    print(f'  ffmpeg check error: {e}')

# Stats: how many videos have no thumbnail file on disk?
print('\n=== Video thumbnail coverage ===')
all_vids = cur.execute(
    "SELECT id, path, thumbnail_path FROM photos WHERE media_type='video'"
).fetchall()
print(f'Total videos: {len(all_vids)}')
no_thumb_in_db = 0
file_ok_no_thumb_file = 0
file_missing = 0
all_ok = 0
for r in all_vids:
    file_ok = Path(r['path']).exists()
    if not file_ok:
        file_missing += 1
        continue
    if not r['thumbnail_path']:
        no_thumb_in_db += 1
        continue
    thumb_ok = Path(r['thumbnail_path']).exists()
    if not thumb_ok:
        file_ok_no_thumb_file += 1
    else:
        all_ok += 1
print(f'  All OK (file + thumb both present):       {all_ok}')
print(f'  File OK but thumbnail_path is NULL:      {no_thumb_in_db}')
print(f'  File OK but thumb file missing on disk:  {file_ok_no_thumb_file}')
print(f'  File missing on disk (orphan):           {file_missing}')

conn.close()
