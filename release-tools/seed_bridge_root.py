"""
v1.5.225 — Pre-seed bridge_root so the user doesn't have to re-pick
the folder in Settings. The actual table is `settings` (kv pair).

Safe to run with the app running — it does a single UPSERT.
"""
import sqlite3
DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
conn = sqlite3.connect(DB, timeout=10)
cur = conn.cursor()
cur.execute(
    "INSERT INTO settings(key, value) VALUES('bridge_root', ?) "
    "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    (r'D:\Fotograflar',)
)
conn.commit()
row = cur.execute("SELECT value FROM settings WHERE key='bridge_root'").fetchone()
print(f'bridge_root seeded: {row[0] if row else None}')
# Also clean the stray app_settings row we wrote earlier
cur.execute("DELETE FROM app_settings WHERE key='bridge_root'")
conn.commit()
conn.close()
