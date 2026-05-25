import sqlite3

db = r'D:\Fotograflar\RetinaTag\retina.db'
conn = sqlite3.connect(db)

total = conn.execute("SELECT COUNT(*) FROM photos").fetchone()[0]
bad = conn.execute(r"SELECT COUNT(*) FROM photos WHERE path LIKE '%\thumbnails\%'").fetchone()[0]
bad_slash = conn.execute("SELECT COUNT(*) FROM photos WHERE path LIKE '%/thumbnails/%'").fetchone()[0]
inside_retinatag = conn.execute(r"SELECT COUNT(*) FROM photos WHERE path LIKE '%\RetinaTag\thumbnails\%'").fetchone()[0]

print("Total photos rows:        ", total)
print("Rows path \\thumbnails\\: ", bad)
print("Rows path /thumbnails/:   ", bad_slash)
print("Rows path \\RetinaTag\\thumbnails\\:", inside_retinatag)

print("\nSample BAD paths:")
for r in conn.execute(r"SELECT path, width, height FROM photos WHERE path LIKE '%\thumbnails\%' LIMIT 3"):
    print(" ", r)

print("\nSample REAL photo paths:")
for r in conn.execute(r"SELECT path, width, height FROM photos WHERE path NOT LIKE '%\thumbnails\%' ORDER BY id DESC LIMIT 5"):
    print(" ", r)
