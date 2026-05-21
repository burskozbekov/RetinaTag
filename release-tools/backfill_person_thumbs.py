"""
v1.5.227 — Backfill persons.thumbnail for every row whose value is
NULL but whose person has at least one assigned face_region with a
matching face_<id>.jpg on disk.

Why this is needed: face detection writes face_<id>.jpg under
thumbnails/faces/ for every detection, AND assign_face_to_person
stamps that filename into persons.thumbnail. But the bulk paths
(batch_assign_person, auto-merge clustering, Mac sync of person
rows) skip that stamp, so the sidebar avatars show up as generic
grey circles even though the actual crops exist on disk.

Run with the app CLOSED so the DB isn't locked.
"""
import sqlite3, sys, io
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
FACES_DIR = Path(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\thumbnails\faces')

conn = sqlite3.connect(DB, timeout=30)
cur = conn.cursor()

# Sanity: faces dir exists
if not FACES_DIR.exists():
    print(f'Faces dir not found: {FACES_DIR}')
    sys.exit(1)

# Index every face_<id>.jpg file we have on disk
disk_face_ids = set()
for f in FACES_DIR.iterdir():
    if f.suffix.lower() == '.jpg' and f.stem.startswith('face_'):
        try:
            disk_face_ids.add(int(f.stem.split('_', 1)[1]))
        except ValueError:
            pass
print(f'Found {len(disk_face_ids):,} face_*.jpg files on disk')

# Every person with NULL thumbnail
needy = cur.execute(
    "SELECT id, name FROM persons WHERE thumbnail IS NULL OR thumbnail = ''"
).fetchall()
print(f'Persons missing thumbnail: {len(needy)}')

patched = 0
no_face_on_disk = 0
no_assigned_faces = 0
for pid, name in needy:
    # Get their assigned faces, ordered by score descending so we pick
    # the highest-confidence detection as the representative thumbnail.
    rows = cur.execute(
        """SELECT id, score FROM face_regions
            WHERE person_id = ?
            ORDER BY score DESC""",
        (pid,)
    ).fetchall()
    if not rows:
        no_assigned_faces += 1
        continue

    # Pick the highest-score face whose crop exists on disk
    chosen = None
    for face_id, _score in rows:
        if face_id in disk_face_ids:
            chosen = face_id
            break

    if chosen is None:
        no_face_on_disk += 1
        continue

    thumb_name = f'face_{chosen}.jpg'
    cur.execute(
        "UPDATE persons SET thumbnail = ? WHERE id = ?",
        (thumb_name, pid)
    )
    patched += 1

conn.commit()

# Verify
after = cur.execute(
    "SELECT COUNT(*) FROM persons WHERE thumbnail IS NOT NULL AND thumbnail != ''"
).fetchone()[0]
total = cur.execute("SELECT COUNT(*) FROM persons").fetchone()[0]

print()
print('===== Done =====')
print(f'Patched:                {patched}')
print(f'No crop on disk:        {no_face_on_disk}')
print(f'No assigned faces:      {no_assigned_faces}')
print(f'Final coverage:         {after} / {total}')

conn.close()
