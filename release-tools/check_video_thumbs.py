"""
v1.5.237 — Why aren't video thumbnails generated?
"""
import sqlite3, sys, io
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
THUMBS_DIR = Path(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\thumbnails')

conn = sqlite3.connect(DB, timeout=10)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

print('=== Video stats ===')
total_videos = cur.execute("SELECT COUNT(*) FROM photos WHERE media_type='video'").fetchone()[0]
print(f'Total videos in DB: {total_videos}')

print()
print('=== Files from screenshot ===')
for name in ('CWSW1852.MP4', 'IMG_5233.MOV', 'IMG_E3613.MOV', 'CGNT6692.MOV', 'IMG_5563.MOV'):
    rows = cur.execute(
        "SELECT id, path, hash, media_type, thumbnail_path FROM photos WHERE filename = ? OR path LIKE ?",
        (name, '%' + name)
    ).fetchall()
    for r in rows:
        thumb = r['thumbnail_path']
        thumb_exists = Path(thumb).exists() if thumb else False
        # Check the "raw" thumb file by hash
        cache_name = (r['hash'][:8] + '.jpg') if r['hash'] else '?'
        cache_path = THUMBS_DIR / cache_name
        cache_exists = cache_path.exists()
        path_exists = Path(r['path']).exists()
        print(f'  id={r["id"]:>5} type={r["media_type"]:<5} file_exists={path_exists}')
        print(f'    path = {r["path"]}')
        print(f'    hash = {r["hash"]}')
        print(f'    thumbnail_path in DB = {thumb} (file_exists={thumb_exists})')
        print(f'    cache hash-based = {cache_path.name} (file_exists={cache_exists})')

# Count of videos with thumbnails
print()
with_thumb = cur.execute(
    "SELECT COUNT(*) FROM photos WHERE media_type='video' AND thumbnail_path IS NOT NULL AND thumbnail_path != ''"
).fetchone()[0]
print(f'Videos with thumbnail_path populated: {with_thumb} / {total_videos}')

# Sample a few with thumbnail_path null
print()
print('Sample videos with NULL thumbnail_path:')
samples = cur.execute(
    "SELECT id, path, hash FROM photos WHERE media_type='video' AND (thumbnail_path IS NULL OR thumbnail_path = '') LIMIT 5"
).fetchall()
for r in samples:
    print(f'  id={r["id"]} hash={r["hash"][:16] if r["hash"] else None} path={r["path"]}')

conn.close()
