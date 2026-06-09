import sqlite3, sys, os
sys.stdout.reconfigure(encoding='utf-8')
db = os.path.expandvars(r'%TEMP%\retina_probe.db')
c = sqlite3.connect(db)
cur = c.cursor()

print('--- Tag stats BEFORE backfill ---')
total_photos = cur.execute('SELECT COUNT(*) FROM photos').fetchone()[0]
tagged_photos = cur.execute('SELECT COUNT(DISTINCT photo_id) FROM tags').fetchone()[0]
total_tags = cur.execute('SELECT COUNT(*) FROM tags').fetchone()[0]
xmp_tags = cur.execute("SELECT COUNT(*) FROM tags WHERE source='xmp_sidecar'").fetchone()[0]
print(f'  photos:         {total_photos:>8,}')
print(f'  tagged photos:  {tagged_photos:>8,}')
print(f'  total tag rows: {total_tags:>8,}')
print(f'  xmp_sidecar:    {xmp_tags:>8,}')

# Check a specific photo we know has sidecar: 5205.MP4
row = cur.execute("SELECT id, path FROM photos WHERE path LIKE '%5205.%' LIMIT 5").fetchone()
if row:
    pid, path = row
    print(f'\n  Photo: {path} (id={pid})')
    tags = cur.execute('SELECT tag, source FROM tags WHERE photo_id=?', (pid,)).fetchall()
    print(f'  Current tags ({len(tags)}):')
    for t, s in tags:
        print(f'    [{s}] {t}')
