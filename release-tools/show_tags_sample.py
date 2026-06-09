import sqlite3, sys, os
sys.stdout.reconfigure(encoding='utf-8')
db = os.path.expandvars(r'%TEMP%\retina_probe\retina.db')
c = sqlite3.connect(db)
cur = c.cursor()

# Find a Mac-tagged photo (xmp_sidecar source) and show all its tags
print('=== IMG_3393.JPG (Ali Can Bombadil photo) ===')
pid_row = cur.execute("SELECT id, path FROM photos WHERE path LIKE '%IMG_3393.JPG'").fetchone()
if pid_row:
    pid, path = pid_row
    print(f'Photo: {path}  (id={pid})')
    desc = cur.execute('SELECT description FROM photos WHERE id=?', (pid,)).fetchone()
    if desc and desc[0]:
        print(f'\nMac AI Description:')
        print(f'  "{desc[0]}"')
    rating = cur.execute('SELECT rating, favorite FROM photos WHERE id=?', (pid,)).fetchone()
    if rating:
        print(f'\nRating: {rating[0]}  Favorite: {bool(rating[1])}')
    print(f'\nTags ({cur.execute("SELECT COUNT(*) FROM tags WHERE photo_id=?", (pid,)).fetchone()[0]}):')
    for tag, src in cur.execute('SELECT tag, source FROM tags WHERE photo_id=? ORDER BY source, tag', (pid,)):
        print(f'  [{src:<14}] {tag}')
    print(f'\nFace regions ({cur.execute("SELECT COUNT(*) FROM face_regions WHERE photo_id=?", (pid,)).fetchone()[0]}):')
    for fr in cur.execute("""
        SELECT pe.name, fr.x1, fr.y1, fr.x2, fr.y2,
               CASE WHEN fr.embedding IS NULL THEN 'xmp' ELSE 'detected' END as src
        FROM face_regions fr LEFT JOIN persons pe ON pe.id = fr.person_id
        WHERE fr.photo_id=? ORDER BY pe.name
    """, (pid,)):
        print(f'  [{fr[5]:<8}] {fr[0]:<30} ({fr[1]},{fr[2]}) -> ({fr[3]},{fr[4]})')

print()
print('=== Random Mac-tagged photo from /2024-10-09 ===')
row = cur.execute("""
    SELECT p.id, p.path, p.description FROM photos p
    WHERE p.path LIKE '%2024-10-09%'
      AND EXISTS (SELECT 1 FROM tags t WHERE t.photo_id=p.id AND t.source='xmp_sidecar')
    LIMIT 1
""").fetchone()
if row:
    pid, path, desc = row
    print(f'Photo: {path}')
    if desc:
        print(f'Description: "{desc[:100]}..."')
    tags = cur.execute("SELECT tag FROM tags WHERE photo_id=? AND source='xmp_sidecar' LIMIT 10", (pid,)).fetchall()
    print(f'Mac-imported tags (first 10 of {cur.execute("SELECT COUNT(*) FROM tags WHERE photo_id=? AND source=?", (pid, "xmp_sidecar")).fetchone()[0]}):')
    for t in tags:
        print(f'  - {t[0]}')

print()
print('=== System totals ===')
print(f'  Total photos:           {cur.execute("SELECT COUNT(*) FROM photos").fetchone()[0]:>8,}')
print(f'  Total tagged photos:    {cur.execute("SELECT COUNT(DISTINCT photo_id) FROM tags").fetchone()[0]:>8,}')
print(f'  Tags from Mac (xmp):    {cur.execute("SELECT COUNT(*) FROM tags WHERE source=?", ("xmp_sidecar",)).fetchone()[0]:>8,}')
print(f'  Tags from Windows (AI): {cur.execute("SELECT COUNT(*) FROM tags WHERE source=?", ("local",)).fetchone()[0]:>8,}')
print(f'  Face regions from Mac:  {cur.execute("SELECT COUNT(*) FROM face_regions WHERE embedding IS NULL").fetchone()[0]:>8,}')
print(f'  Face regions detected:  {cur.execute("SELECT COUNT(*) FROM face_regions WHERE embedding IS NOT NULL").fetchone()[0]:>8,}')
print(f'  Total persons:          {cur.execute("SELECT COUNT(*) FROM persons").fetchone()[0]:>8,}')
