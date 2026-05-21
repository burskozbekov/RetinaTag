"""
v1.5.227 — Stricter Serdar cleanup. Use ABSOLUTE cosine threshold
(0.55) instead of relative stdev. Anything below 0.55 is almost
certainly a different person — that's the cosine band where two
faces of the SAME person should never land for a healthy detector.

Also drops the "Serdar" face tag from any photo whose ALL remaining
face_regions fall below the threshold — the photo just isn't Serdar.
"""
import sqlite3, sys, io, struct, math, json, datetime
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
NAME = 'Serdar'
COS_THRESHOLD = 0.55   # below this, not the same person

conn = sqlite3.connect(DB, timeout=30)
cur = conn.cursor()
pid = cur.execute("SELECT id FROM persons WHERE name = ?", (NAME,)).fetchone()[0]

rows = cur.execute(
    "SELECT id, photo_id, embedding FROM face_regions WHERE person_id = ?",
    (pid,)
).fetchall()
print(f'{NAME!r} face_regions: {len(rows)}')

def emb_of(b):
    n = len(b) // 4
    return list(struct.unpack(f'{n}f', b))
def dot(a, b): return sum(x*y for x, y in zip(a, b))
def norm(a):   return math.sqrt(sum(x*x for x in a))

embs = [(fid, photo_id, emb_of(b)) for (fid, photo_id, b) in rows if b]
d = len(embs[0][2])

# Recompute centroid from the HIGH-confidence subset only — using all
# 605 inflates the centroid toward whatever non-Serdar faces are in
# there. Bootstrap: start with overall centroid, drop bottom 20%,
# recompute. Two passes converge for clean enough clusters.
def centroid_of(es):
    return [sum(e[2][i] for e in es) / len(es) for i in range(d)]

cur_set = embs
for _ in range(2):
    c = centroid_of(cur_set)
    cn = norm(c)
    sims = []
    for fid, pid_, e in cur_set:
        en = norm(e)
        if en == 0 or cn == 0: continue
        sims.append((dot(e, c) / (en * cn), fid, pid_, e))
    sims.sort(reverse=True)
    keep = max(int(len(sims) * 0.8), 50)
    cur_set = [(fid, pid_, e) for _cos, fid, pid_, e in sims[:keep]]

centroid = centroid_of(cur_set)
cnorm = norm(centroid)
print(f'Bootstrapped centroid from cleanest {len(cur_set)}/{len(embs)} faces')

# Now score ALL 605 against this cleaner centroid
sims = []
for fid, photo_id, e in embs:
    en = norm(e)
    if en == 0 or cnorm == 0: continue
    sims.append((dot(e, centroid) / (en * cnorm), fid, photo_id))
sims.sort()

below = [(c, f, p) for c, f, p in sims if c < COS_THRESHOLD]
print(f'Below cos={COS_THRESHOLD}: {len(below)} faces')

if not below:
    print('Cluster is clean by absolute threshold.'); sys.exit(0)

# Show worst 30
print()
print('Worst 30:')
for c, f, p in below[:30]:
    pa = cur.execute("SELECT path FROM photos WHERE id = ?", (p,)).fetchone()
    fn = pa[0].split('\\')[-1] if pa else '?'
    print(f'  cos={c:.3f}  face_id={f:>6}  photo_id={p:>6}  {fn}')

# Apply
ids_to_clean = [f for _c, f, _p in below]
photos_touched = set(p for _c, _f, p in below)

ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup_path = Path(DB).parent / f'serdar_strict_backup_{ts}.json'
backup = [{'face_id': f, 'photo_id': p, 'cos_to_centroid': c} for c, f, p in below]
backup_path.write_text(json.dumps(backup, indent=2), encoding='utf-8')
print(f'\nBackup -> {backup_path}')

# Unassign these face_regions
placeholders = ','.join('?' * len(ids_to_clean))
cur.execute(
    f"UPDATE face_regions SET person_id = NULL WHERE id IN ({placeholders}) AND person_id = ?",
    (*ids_to_clean, pid)
)
n_face_changed = cur.rowcount
print(f'face_regions unassigned: {n_face_changed}')

# Drop face-kind "Serdar" tags from any photo whose REMAINING
# face_regions are ALL below threshold (or zero) — i.e., the photo
# doesn't contain Serdar.
n_tags_removed = 0
for photo_id in photos_touched:
    remaining_face_ids = [r[0] for r in cur.execute(
        "SELECT id FROM face_regions WHERE photo_id = ? AND person_id = ?",
        (photo_id, pid)
    ).fetchall()]
    if not remaining_face_ids:
        # No Serdar face left on this photo at all
        res = cur.execute(
            "DELETE FROM tags WHERE photo_id = ? AND tag = ? AND source = 'face'",
            (photo_id, NAME)
        )
        n_tags_removed += res.rowcount
print(f'photo "Serdar" tags removed: {n_tags_removed}')

conn.commit()
left = cur.execute("SELECT COUNT(*) FROM face_regions WHERE person_id = ?", (pid,)).fetchone()[0]
print(f'\nSerdar still linked to {left} face_regions (was {len(rows)})')
conn.close()
