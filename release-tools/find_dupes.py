import sqlite3, os, sys
sys.stdout.reconfigure(encoding='utf-8')
db = os.path.expandvars(r'%TEMP%\retina_probe\retina.db')
c = sqlite3.connect(db)
cur = c.cursor()

print('=== Case-insensitive duplicates within same photo ===')
print('  (photos that have e.g. "Buğra" AND "buğra" both as separate rows)')
rows = list(cur.execute("""
    SELECT photo_id, LOWER(tag) as tlow, COUNT(*) as n, GROUP_CONCAT(tag||':'||source, ', ')
    FROM tags
    GROUP BY photo_id, LOWER(tag)
    HAVING COUNT(*) > 1
    LIMIT 30
"""))
total_dup_groups = cur.execute("""
    SELECT COUNT(*) FROM (
        SELECT photo_id, LOWER(tag), COUNT(*) FROM tags
        GROUP BY photo_id, LOWER(tag) HAVING COUNT(*) > 1
    )
""").fetchone()[0]
print(f'Total duplicate groups (photo, lowered_tag): {total_dup_groups}')
print(f'First 30:')
for pid, tlow, n, variants in rows:
    print(f'  photo {pid}  "{tlow}"  ({n} rows): {variants}')

print()
print('=== Distinct tag count: byte-exact vs case-insensitive ===')
exact = cur.execute("SELECT COUNT(DISTINCT tag) FROM tags").fetchone()[0]
ci = cur.execute("SELECT COUNT(DISTINCT LOWER(tag)) FROM tags").fetchone()[0]
print(f'  byte-exact:        {exact:,}')
print(f'  case-insensitive:  {ci:,}')
print(f'  excess (variants): {exact - ci:,}')

print()
print('=== Top 20 tag variant collisions across library ===')
print('  (same tag in different cases, counted as separate)')
for r in cur.execute("""
    SELECT LOWER(tag), COUNT(DISTINCT tag), GROUP_CONCAT(DISTINCT tag) FROM tags
    GROUP BY LOWER(tag)
    HAVING COUNT(DISTINCT tag) > 1
    ORDER BY COUNT(DISTINCT tag) DESC
    LIMIT 20
"""):
    print(f'  "{r[0]}"  variants={r[1]}  → {r[2]}')
