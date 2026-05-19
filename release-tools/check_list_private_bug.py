import sqlite3
c = sqlite3.connect(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')

# Repro the exact two-step query that list_private_photos does.
# Step 1: ids = SELECT id WHERE private=1
ids = [r[0] for r in c.execute("SELECT id FROM photos WHERE private = 1 ORDER BY COALESCE(date_taken, created_at) DESC LIMIT 5000").fetchall()]
print(f'Step 1 — private=1 ids: {len(ids)}')

if ids:
    # Step 2: get_photos_by_ids with WHERE p.private = 0
    placeholders = ','.join('?' for _ in ids)
    sql = f"""SELECT p.id, p.filename FROM photos p
              LEFT JOIN tags t ON t.photo_id = p.id
              WHERE p.id IN ({placeholders}) AND p.private = 0
              GROUP BY p.id"""
    rows = c.execute(sql, ids).fetchall()
    print(f'Step 2 (with private=0 filter): {len(rows)} rows returned')
    print('--> CONFIRMED bug: list_private_photos returns empty even when 6 rows exist with private=1')
