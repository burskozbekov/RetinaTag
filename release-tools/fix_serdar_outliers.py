"""
v1.5.227 — One-shot Serdar cluster cleanup.
For every face_region currently linked to person "Serdar" whose
embedding sits more than 2 stdev below the cluster's mean cosine to
its centroid, unassign it (person_id := NULL) AND drop the matching
"face"-kind tag from the photo if no other face_region keeps Serdar
linked to that photo.

Reversible: prints the face_id list and remembers it in a sidecar
JSON so we can revert if needed.
"""
import sqlite3, sys, io, struct, math, json, datetime
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
NAME = 'Serdar'
THRESHOLD_SIGMAS = 2.0  # cos < mean - 2*stdev → outlier

conn = sqlite3.connect(DB, timeout=30)
cur = conn.cursor()

row = cur.execute("SELECT id FROM persons WHERE name = ?", (NAME,)).fetchone()
if not row:
    print(f'No person {NAME!r}'); sys.exit(1)
pid = row[0]

rows = cur.execute(
    "SELECT id, photo_id, embedding FROM face_regions WHERE person_id = ?",
    (pid,)
).fetchall()
print(f'{NAME!r} has {len(rows)} face_regions in DB')

def emb_of(b):
    n = len(b) // 4
    return list(struct.unpack(f'{n}f', b))

def dot(a, b): return sum(x*y for x, y in zip(a, b))
def norm(a):   return math.sqrt(sum(x*x for x in a))

embs = [(fid, photo_id, emb_of(b)) for (fid, photo_id, b) in rows if b]
if not embs:
    print('no embeddings to analyse'); sys.exit(1)

d = len(embs[0][2])
centroid = [sum(e[2][i] for e in embs) / len(embs) for i in range(d)]
cnorm = norm(centroid)

sims = []
for fid, photo_id, e in embs:
    en = norm(e)
    if en == 0 or cnorm == 0:
        continue
    cos = dot(e, centroid) / (en * cnorm)
    sims.append((cos, fid, photo_id))

import statistics
sims.sort()
mean_c = statistics.mean(s[0] for s in sims)
sd_c   = statistics.stdev(s[0] for s in sims) if len(sims) > 1 else 0
thr    = mean_c - THRESHOLD_SIGMAS * sd_c
print(f'cosine: mean={mean_c:.3f} stdev={sd_c:.3f} threshold={thr:.3f}')

outliers = [(cos, fid, photo_id) for cos, fid, photo_id in sims if cos < thr]
print(f'Outliers below threshold: {len(outliers)}')
for cos, fid, photo_id in outliers:
    p = cur.execute("SELECT path FROM photos WHERE id = ?", (photo_id,)).fetchone()
    fname = (p[0].split('\\')[-1]) if p else '?'
    print(f'  cos={cos:.3f}  face_id={fid:>6}  photo_id={photo_id:>6}  {fname}')

if not outliers:
    print('Nothing to clean.'); sys.exit(0)

# Backup the to-be-changed rows
backup_path = Path(DB).parent / f'serdar_outliers_backup_{datetime.datetime.now().strftime("%Y%m%d-%H%M%S")}.json'
backup = []
photo_ids_touched = set()
for _cos, fid, photo_id in outliers:
    row = cur.execute(
        "SELECT id, person_id FROM face_regions WHERE id = ?",
        (fid,)
    ).fetchone()
    backup.append({'face_id': fid, 'old_person_id': row[1] if row else None, 'photo_id': photo_id})
    photo_ids_touched.add(photo_id)

backup_path.write_text(json.dumps(backup, indent=2), encoding='utf-8')
print(f'\nBackup saved to: {backup_path}')

# Apply: unassign these face_regions from Serdar.
face_ids = [fid for _c, fid, _p in outliers]
placeholders = ','.join('?' * len(face_ids))
cur.execute(
    f"UPDATE face_regions SET person_id = NULL WHERE id IN ({placeholders}) AND person_id = ?",
    (*face_ids, pid)
)
n_face_changed = cur.rowcount

# For each touched photo, check if Serdar still has any face_region there.
# If not, remove the "Serdar" face-kind tag from that photo so the photo
# stops showing up in person-filter searches.
n_tags_removed = 0
for photo_id in photo_ids_touched:
    still_serdar = cur.execute(
        "SELECT 1 FROM face_regions WHERE photo_id = ? AND person_id = ? LIMIT 1",
        (photo_id, pid)
    ).fetchone()
    if still_serdar:
        continue  # other face_regions on this photo still link Serdar; keep the tag
    res = cur.execute(
        "DELETE FROM tags WHERE photo_id = ? AND tag = ? AND source = 'face'",
        (photo_id, NAME)
    )
    n_tags_removed += res.rowcount

conn.commit()

# Verify
left = cur.execute(
    "SELECT COUNT(*) FROM face_regions WHERE person_id = ?", (pid,)
).fetchone()[0]
print(f'\n===== Done =====')
print(f'face_regions unassigned: {n_face_changed}')
print(f'photo "Serdar" tags removed (no remaining face_region): {n_tags_removed}')
print(f'Serdar still linked to {left} face_regions (was {len(rows)})')

conn.close()
