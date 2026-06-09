import sqlite3, sys, os
sys.stdout.reconfigure(encoding='utf-8')
db = os.path.expandvars(r'%TEMP%\retina_probe\retina.db')
c = sqlite3.connect(db)
cur = c.cursor()

print('=== Person face counts ===')
for r in cur.execute("""
    SELECT pe.name, COUNT(fr.id) as faces,
           SUM(CASE WHEN fr.embedding IS NULL THEN 1 ELSE 0 END) as imported,
           SUM(CASE WHEN fr.embedding IS NOT NULL THEN 1 ELSE 0 END) as detected
    FROM persons pe
    LEFT JOIN face_regions fr ON fr.person_id = pe.id
    GROUP BY pe.id
    ORDER BY faces DESC
    LIMIT 20
"""):
    print(f'  {r[1]:>5} faces  ({r[2]:>4} imported, {r[3]:>4} detected)  {r[0]}')

print()
print('=== Person filter for "Ali Can Bombadil" via new search_photos_by_person SQL ===')
photos = list(cur.execute("""
    SELECT DISTINCT p.id, p.path
    FROM photos p
    LEFT JOIN face_regions fr ON fr.photo_id = p.id
    LEFT JOIN persons pe ON pe.id = fr.person_id
    LEFT JOIN tags t_person ON t_person.photo_id = p.id AND t_person.tag = ? COLLATE NOCASE
    WHERE (pe.name LIKE ? COLLATE NOCASE OR t_person.id IS NOT NULL)
      AND p.private = 0
""", ("Ali Can Bombadil", "%Ali Can Bombadil%")))
print(f'  hits: {len(photos)}')
for pid, path in photos[:10]:
    print(f'    {path}')
