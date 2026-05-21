"""
v1.5.224 — Clean up DB rows whose `path` no longer exists on disk.

These are the leftover-from-migration cases where the DB still has a
row pointing at the OLD Unknown/Unknown location, but the file was
moved (or replaced with a same-named different file) and the row
ended up dangling.

For each orphan row:
  - Try to find a same-name file elsewhere under the library root.
  - If found AND size matches → patch the row to the new location.
  - Otherwise → delete the orphan row. The 'real' file on disk (if
    any) will get re-indexed by the next library scan with fresh
    EXIF and a fresh DB row.

Backs up the DB first. Read-only on disk — never deletes a file.
"""
import sqlite3, shutil, datetime, os
from pathlib import Path

DB = Path(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')
ROOT = Path(r'D:\Fotograflar')

if not DB.exists():
    print(f'DB not found at {DB}')
    raise SystemExit(1)

# Back up DB
ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = DB.with_name(f'retina.db.bak-{ts}-orphan-cleanup')
print(f'Backup -> {backup}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(str(DB), timeout=30)
cur = conn.cursor()

# Find orphan rows
rows = cur.execute("SELECT id, path, size FROM photos").fetchall()
orphans = [(rid, p, sz) for (rid, p, sz) in rows if not Path(p).exists()]
print(f'Orphan DB rows (file missing on disk): {len(orphans)}')

# Build a filename index of what's actually on disk
print('Indexing files on disk...')
fn_index = {}  # name (lower) -> [(path, size), ...]
for d in ROOT.rglob('*'):
    if d.is_file():
        key = d.name.lower()
        fn_index.setdefault(key, []).append((str(d), d.stat().st_size))
print(f'Indexed {sum(len(v) for v in fn_index.values()):,} files')

repaired = 0
deleted = 0
keep_for_review = 0

for rid, old_path, db_size in orphans:
    name = Path(old_path).name.lower()
    candidates = fn_index.get(name, [])
    # Prefer same-size match (likely the same content, just moved)
    same_size = [c for c in candidates if c[1] == db_size]
    if same_size:
        new_path, _ = same_size[0]
        new_folder = str(Path(new_path).parent)
        try:
            cur.execute(
                'UPDATE photos SET path = ?, folder = ? WHERE id = ?',
                (new_path, new_folder, rid)
            )
            repaired += 1
            print(f'  REPAIRED id={rid}: {old_path} -> {new_path}')
            continue
        except sqlite3.IntegrityError:
            # Another DB row already owns that path. The current
            # orphan is a duplicate — drop it.
            pass
    # No same-size candidate. Maybe a different-content file with the
    # same name exists; that's a different photo, so we should NOT
    # repoint the row at it. Just delete the orphan row — a fresh
    # scan will index the real file properly.
    cur.execute('DELETE FROM photos WHERE id = ?', (rid,))
    # Best-effort sweep of auxiliary rows. Schema varies by version
    # (face_clusters / faces / photo_faces etc.), so wrap each in a
    # try and let nonexistent tables slide.
    for aux in ('tags', 'faces', 'face_clusters', 'photo_faces', 'mtp_imports'):
        try:
            cur.execute(f'DELETE FROM {aux} WHERE photo_id = ?', (rid,))
        except sqlite3.OperationalError:
            pass
    deleted += 1
    print(f'  DELETED id={rid}: {old_path}')

conn.commit()
conn.close()

print()
print('===== Done =====')
print(f'Repaired (path patched):  {repaired}')
print(f'Deleted (no real file):   {deleted}')
print(f'DB backup at: {backup}')
print()
if deleted:
    print('Tip: open RetinaTag, run a scan on D:\\Fotograflar to pick up any')
    print('     same-named-but-different files as fresh entries.')
