import sqlite3, os
c = sqlite3.connect(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')
print('--- 6 private photos ---')
rows = c.execute("""
  SELECT id, filename, path, private_thumb_enc IS NOT NULL AS has_enc, vault_folder_id
  FROM photos WHERE private=1 ORDER BY id
""").fetchall()
for r in rows:
    print(f'id={r[0]} fn={r[1][:30]} path_ext={r[2].split(".")[-1] if "." in r[2] else "?"} has_enc_thumb={bool(r[3])} folder_id={r[4]}')
    # Check if path exists on disk
    print(f'    path exists: {os.path.exists(r[2])}')
