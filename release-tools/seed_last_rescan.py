import sqlite3, time
DB = r"D:\Fotograflar\RetinaTag\retina.db"
c = sqlite3.connect(DB, timeout=15)
c.execute("CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
c.execute(
    "INSERT INTO settings(key,value) VALUES('last_rescan_at',?) "
    "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
    (str(int(time.time())),),
)
c.commit()
c.close()
print("seeded last_rescan_at")
