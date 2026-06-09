import sqlite3, os, sys
sys.stdout.reconfigure(encoding='utf-8')
db = os.path.expandvars(r'%TEMP%\retina_probe\retina.db')
c = sqlite3.connect(db)
cur = c.cursor()

print('=== Photo status distribution ===')
for r in cur.execute("SELECT status, COUNT(*) FROM photos GROUP BY status"):
    print(f'  {r[1]:>8,}  {r[0]}')

print()
print('=== Tag-having photos by status ===')
print('  (photos that have at least 1 tag, grouped by their status column)')
for r in cur.execute("""
    SELECT p.status, COUNT(DISTINCT p.id)
    FROM photos p JOIN tags t ON t.photo_id = p.id
    GROUP BY p.status
"""):
    print(f'  {r[1]:>8,}  {r[0]}')

print()
print('=== Photos with ONLY xmp_sidecar tags (no AI tag) ===')
r = cur.execute("""
    SELECT COUNT(*) FROM (
        SELECT p.id, p.status
        FROM photos p
        WHERE EXISTS (SELECT 1 FROM tags WHERE photo_id=p.id AND source='xmp_sidecar')
          AND NOT EXISTS (SELECT 1 FROM tags WHERE photo_id=p.id AND source='local')
    )
""").fetchone()
print(f'  Photos tagged ONLY by Mac (no Windows AI yet): {r[0]:,}')
r2 = cur.execute("""
    SELECT p.status, COUNT(*) FROM photos p
    WHERE EXISTS (SELECT 1 FROM tags WHERE photo_id=p.id AND source='xmp_sidecar')
      AND NOT EXISTS (SELECT 1 FROM tags WHERE photo_id=p.id AND source='local')
    GROUP BY p.status
""").fetchall()
for s, n in r2:
    print(f'    {n:>6,}  status={s}')

print()
print('=== get_stats SQL equivalent (what sidebar shows) ===')
total = cur.execute("SELECT COUNT(*) FROM photos").fetchone()[0]
tagged = cur.execute("SELECT COUNT(*) FROM photos WHERE status='tagged'").fetchone()[0]
pending = cur.execute("SELECT COUNT(*) FROM photos WHERE status='pending'").fetchone()[0]
print(f'  Total:    {total:,}')
print(f'  Tagged:   {tagged:,}  ← sidebar shows this')
print(f'  Pending:  {pending:,}')
