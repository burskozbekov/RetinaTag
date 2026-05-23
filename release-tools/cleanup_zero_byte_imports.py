"""
v1.5.245 — Kill 0-byte / file-missing rows from the latest
broken import burst. Cascades tags / face_regions / mtp_imports
just like the earlier orphan cleanup.
"""
import sqlite3, sys, io, datetime, shutil
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-zero-byte-{ts}')
print(f'Backing up → {backup.name}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=30)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

# Rows where the file is either missing OR exists with 0 bytes.
victims = []
for r in cur.execute("SELECT id, path, filename, size FROM photos").fetchall():
    p = Path(r['path'])
    if not p.exists():
        victims.append((r['id'], r['path'], r['filename'], 'missing'))
        continue
    try:
        if p.stat().st_size == 0:
            victims.append((r['id'], r['path'], r['filename'], 'zero'))
    except OSError:
        victims.append((r['id'], r['path'], r['filename'], 'unreadable'))

print(f'Broken rows (missing / zero / unreadable): {len(victims)}')
if not victims:
    sys.exit(0)
for rid, path, fn, why in victims[:30]:
    print(f'  id={rid:>5} [{why}] {fn} → {path}')
if len(victims) > 30:
    print(f'  …+{len(victims) - 30} more')

# Also delete the 0-byte file from disk so it doesn't keep cluttering
# the bucket. (Missing files don't need touching.)
disk_removed = 0
for rid, path, fn, why in victims:
    if why == 'zero':
        try:
            Path(path).unlink()
            disk_removed += 1
        except OSError:
            pass

# Drop DB rows + cascades
ids_csv = ','.join(str(t[0]) for t in victims)
for table, col in (('tags', 'photo_id'), ('face_regions', 'photo_id'),
                    ('mtp_imports', 'photo_id')):
    try:
        n = cur.execute(f"DELETE FROM {table} WHERE {col} IN ({ids_csv})").rowcount
        if n: print(f'  cascaded {table}: {n} rows')
    except sqlite3.OperationalError:
        pass
n = cur.execute(f"DELETE FROM photos WHERE id IN ({ids_csv})").rowcount
conn.commit()
print()
print(f'Photos rows deleted: {n}')
print(f'0-byte files removed from disk: {disk_removed}')
print(f'Backup: {backup}')
conn.close()
