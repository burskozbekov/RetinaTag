"""
v1.5.237 — Cleanup orphan video DB rows whose file doesn't exist on
disk. Many of the user's videos have a "duplicate" row pointing at a
different month bucket left over from earlier scans / from the
v1.5.221 Unknown rebucket migration. The DB row hangs around with
no thumbnail because the file is gone, and the gallery renders
those as placeholder tiles.

This script:
  1. Finds every photo row whose path doesn't exist on disk.
  2. For each, looks for a sibling row with the same filename whose
     file DOES exist (so we know the user hasn't lost any media).
  3. Deletes the orphan DB row (drops the placeholder tile).
  4. If no sibling exists, the orphan is preserved + logged so the
     user can decide.

Backs up the DB first. Run with the app closed.
"""
import sqlite3, sys, io, datetime, shutil
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

# Backup
ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-orphan-videos-{ts}')
print(f'Backing up DB → {backup.name}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=30)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

# Scan ALL rows for missing files (not just videos — same fix works for
# stranded image rows too).
all_rows = cur.execute(
    "SELECT id, path, filename, hash, media_type FROM photos"
).fetchall()
print(f'Total photos in DB: {len(all_rows):,}')

orphans = []  # (id, path, filename)
for r in all_rows:
    if not Path(r['path']).exists():
        orphans.append((r['id'], r['path'], r['filename']))
print(f'Orphan DB rows (file missing): {len(orphans)}')
if not orphans:
    print('No orphans to clean. Done.')
    sys.exit(0)

# Index live rows by filename → ids (the rows whose file DOES exist)
live_by_name = {}
for r in all_rows:
    if Path(r['path']).exists():
        live_by_name.setdefault(r['filename'], []).append(r['id'])
print(f'Live files indexed by filename: {len(live_by_name):,}')

# Plan the deletes
to_delete = []
no_sibling = []
for rid, path, fname in orphans:
    siblings = live_by_name.get(fname, [])
    if siblings:
        to_delete.append((rid, path, fname, len(siblings)))
    else:
        no_sibling.append((rid, path, fname))

print()
print(f'Orphans with a sibling row that has a live file: {len(to_delete)}')
print(f'Orphans with NO sibling (genuinely lost):       {len(no_sibling)}')

if to_delete:
    print()
    print('Sample (first 10 will be deleted):')
    for rid, path, fname, sib in to_delete[:10]:
        print(f'  id={rid:>5} fname={fname:<30} siblings={sib}  path={path}')

    # Delete the orphans. Use CASCADE — tags, face_regions, etc. tied to
    # these orphan ids should go with them. SQLite default doesn't
    # cascade; do it explicitly.
    ids_csv = ','.join(str(t[0]) for t in to_delete)
    for table, col in (
        ('tags', 'photo_id'),
        ('face_regions', 'photo_id'),
        ('mtp_imports', 'photo_id'),
    ):
        try:
            n = cur.execute(f"DELETE FROM {table} WHERE {col} IN ({ids_csv})").rowcount
            print(f'  cascaded {table}: {n} rows')
        except sqlite3.OperationalError as e:
            print(f'  ({table} skipped: {e})')

    n = cur.execute(f"DELETE FROM photos WHERE id IN ({ids_csv})").rowcount
    print(f'photos rows deleted: {n}')
    conn.commit()
    print('Committed.')

if no_sibling:
    print()
    print('Orphans with NO sibling — preserved (user decision):')
    for rid, path, fname in no_sibling[:20]:
        print(f'  id={rid:>5} {fname:<40} {path}')
    if len(no_sibling) > 20:
        print(f'  …and {len(no_sibling) - 20} more')

conn.close()
print()
print(f'Backup at: {backup}')
