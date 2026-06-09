import sqlite3
db = r'D:\Fotograflar\RetinaTag\retina.db'
conn = sqlite3.connect(db)
candidates = ['IMG_9170.MOV','IMG_9168.MOV','IMG_7684.PNG','IMG_7708.MOV','IMG_7682.JPG']
for fn in candidates:
    rows = conn.execute(
        "SELECT id, path, private, vault_oid FROM photos WHERE filename = ?",
        (fn,)
    ).fetchall()
    if not rows:
        print(f"{fn:18s} → NOT IN PC DB")
    else:
        for r in rows:
            pid, path, private, void = r
            tag = 'VAULT' if (private or void) else 'regular'
            print(f"{fn:18s} id={pid} {tag}  path={path}")
print()
print("PC vault count:", conn.execute(
    "SELECT COUNT(*) FROM photos WHERE private = 1 OR vault_oid IS NOT NULL"
).fetchone()[0])
