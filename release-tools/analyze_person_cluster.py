"""
v1.5.227 — Check how diverse the face embeddings under a given
person are. If the cosine spread is high, the cluster is mixed
(grouped multiple people under one name) and probably needs a
split.
"""
import sqlite3, sys, io, struct, math
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
NAME = 'Serdar'

conn = sqlite3.connect(DB, timeout=10)
cur = conn.cursor()

row = cur.execute("SELECT id FROM persons WHERE name = ?", (NAME,)).fetchone()
if not row:
    print(f'No person named {NAME!r}'); sys.exit(1)
pid = row[0]

# Pull all face embeddings linked to this person
rows = cur.execute(
    "SELECT id, photo_id, embedding FROM face_regions WHERE person_id = ?",
    (pid,)
).fetchall()
print(f'Person {NAME!r} (id={pid}): {len(rows)} face_regions')

def emb_from_bytes(b):
    n = len(b) // 4
    return list(struct.unpack(f'{n}f', b))

def dot(a, b):
    return sum(x*y for x, y in zip(a, b))

def norm(a):
    return math.sqrt(sum(x*x for x in a))

# Compute pairwise cosine similarity vs the cluster centroid
import statistics
embs = [(fid, pid_, emb_from_bytes(b)) for (fid, pid_, b) in rows if b]
print(f'Embeddings parsed: {len(embs)}')
if not embs:
    sys.exit(0)
d = len(embs[0][2])
centroid = [sum(e[2][i] for e in embs) / len(embs) for i in range(d)]
cnorm = norm(centroid)

similarities = []
for fid, photo_id, e in embs:
    en = norm(e)
    if en == 0 or cnorm == 0:
        continue
    cos = dot(e, centroid) / (en * cnorm)
    similarities.append((cos, fid, photo_id))

similarities.sort()  # lowest = most outlier
mean_cos = statistics.mean(s[0] for s in similarities)
med_cos = statistics.median(s[0] for s in similarities)
stdev_cos = statistics.stdev(s[0] for s in similarities) if len(similarities) > 1 else 0

print(f'Cosine to centroid:  mean={mean_cos:.3f}  median={med_cos:.3f}  stdev={stdev_cos:.3f}')
print()
print('20 LEAST-MATCHING faces (likely "not actually Serdar"):')
for cos, fid, photo_id in similarities[:20]:
    pa = cur.execute("SELECT path FROM photos WHERE id = ?", (photo_id,)).fetchone()
    fname = pa[0].split('\\')[-1] if pa else '?'
    print(f'  cos={cos:.3f}  face_id={fid:>6}  photo={fname}')

print()
print('20 MOST-MATCHING faces (definitely Serdar):')
for cos, fid, photo_id in similarities[-20:]:
    pa = cur.execute("SELECT path FROM photos WHERE id = ?", (photo_id,)).fetchone()
    fname = pa[0].split('\\')[-1] if pa else '?'
    print(f'  cos={cos:.3f}  face_id={fid:>6}  photo={fname}')

# Suggest: cosine threshold for "definitely not him"
threshold = mean_cos - 2 * stdev_cos
outliers = sum(1 for s in similarities if s[0] < threshold)
print()
print(f'Faces below mean - 2*stdev ({threshold:.3f}): {outliers}')

conn.close()
