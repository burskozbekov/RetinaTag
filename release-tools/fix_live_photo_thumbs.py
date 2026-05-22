"""
v1.5.240 — Second pass for the 91 iPhone Live Photo MOVs whose
first try in fix_videos_round2.py failed because `-ss 1` sought
past the end (these clips are usually <1 second). Re-run with
`-ss 0` (grab the very first frame) as the fallback.
"""
import sqlite3, sys, io, subprocess
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

conn = sqlite3.connect(DB, timeout=30)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

rows = cur.execute(
    "SELECT id, path, thumbnail_path FROM photos "
    "WHERE media_type='video' AND thumbnail_path IS NOT NULL AND thumbnail_path != ''"
).fetchall()
todo = [(r['id'], r['path'], r['thumbnail_path'])
        for r in rows
        if Path(r['path']).exists() and not Path(r['thumbnail_path']).exists()]
print(f'Videos still missing thumb: {len(todo)}')
if not todo:
    print('Nothing to do — all caught up.')
    sys.exit(0)

ok = 0
fail = 0
for i, (rid, video, thumb) in enumerate(todo, 1):
    if i % 25 == 0 or i == len(todo):
        print(f'  {i:>4}/{len(todo)}  ok={ok}  fail={fail}')
    # Frame 0 (or as early as ffmpeg can decode). Some Live Photo
    # MOVs are < 1 second long so the earlier `-ss 1` overshot.
    args = ['ffmpeg', '-ss', '0', '-i', video, '-frames:v', '1',
            '-vf', 'scale=320:-1', '-q:v', '4', '-y',
            '-loglevel', 'error', thumb]
    try:
        proc = subprocess.run(args, capture_output=True, timeout=20)
        if proc.returncode == 0 and Path(thumb).exists() and Path(thumb).stat().st_size > 0:
            ok += 1
        else:
            fail += 1
    except Exception:
        fail += 1

print(f'\nDone: regenerated={ok} failed={fail}')
conn.close()
