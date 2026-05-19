import sqlite3, os
c = sqlite3.connect(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')

# Find private photos whose path doesn't exist on disk → dead rows
dead = []
for row in c.execute("SELECT id, filename, path, vault_folder_id FROM photos WHERE private=1").fetchall():
    pid, fn, path, fid = row
    if not os.path.exists(path):
        dead.append((pid, fn, path, fid))

print(f'Dead vault rows: {len(dead)}')
for d in dead:
    print(f'  id={d[0]} fn={d[1][:30]} folder_id={d[3]}')

if dead:
    ids = [d[0] for d in dead]
    placeholders = ','.join('?' for _ in ids)
    c.execute(f'DELETE FROM photos WHERE id IN ({placeholders})', ids)
    c.commit()
    print(f'\nDeleted {len(ids)} dead photo rows')

# Now clean up vault_folders rows with 0 photos
orphan_folders = c.execute("""
  SELECT f.id, f.name FROM vault_folders f
  WHERE NOT EXISTS (SELECT 1 FROM photos WHERE vault_folder_id = f.id AND private = 1)
""").fetchall()
print(f'\nOrphan vault_folders rows: {len(orphan_folders)}')
for f in orphan_folders:
    print(f'  id={f[0]} name={f[1]}')

if orphan_folders:
    ids = [f[0] for f in orphan_folders]
    placeholders = ','.join('?' for _ in ids)
    c.execute(f'DELETE FROM vault_folders WHERE id IN ({placeholders})', ids)
    c.commit()
    print(f'Deleted {len(ids)} orphan folder rows')
