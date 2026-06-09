import sqlite3, sys, os
sys.stdout.reconfigure(encoding='utf-8')
db = os.path.expandvars(r'%TEMP%\retina_probe\retina.db')
c = sqlite3.connect(db)
cur = c.cursor()

total_photos = cur.execute('SELECT COUNT(*) FROM photos').fetchone()[0]
tagged_photos = cur.execute('SELECT COUNT(DISTINCT photo_id) FROM tags').fetchone()[0]
total_tags = cur.execute('SELECT COUNT(*) FROM tags').fetchone()[0]
xmp_tags = cur.execute("SELECT COUNT(*) FROM tags WHERE source='xmp_sidecar'").fetchone()[0]
sources = cur.execute("SELECT source, COUNT(*) FROM tags GROUP BY source ORDER BY 2 DESC").fetchall()

print(f'photos:         {total_photos:>8,}')
print(f'tagged photos:  {tagged_photos:>8,}')
print(f'total tag rows: {total_tags:>8,}')
print(f'xmp_sidecar:    {xmp_tags:>8,}')
print()
print('Tag sources:')
for s, n in sources:
    print(f'  {n:>8,}  {s}')

# Verify a specific photo we know has a sidecar
print()
row = cur.execute("SELECT id, path FROM photos WHERE path LIKE '%5205.MP4' LIMIT 1").fetchone()
if row:
    pid, path = row
    print(f'{path} (id={pid})')
    tags = cur.execute('SELECT tag, source FROM tags WHERE photo_id=?', (pid,)).fetchall()
    print(f'  tags ({len(tags)}):')
    for t, s in tags[:30]:
        print(f'    [{s:<14}] {t}')
