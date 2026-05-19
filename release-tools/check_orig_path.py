import sqlite3
c = sqlite3.connect(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')
print('--- 6 private photos: original_path + folder_id ---')
for r in c.execute("""
  SELECT id, filename, path, original_path, vault_folder_id
  FROM photos WHERE private=1 ORDER BY id
""").fetchall():
    print(f'id={r[0]} fn={r[1][:25]}')
    print(f'  path: {r[2]}')
    print(f'  orig: {r[3]}')
    print(f'  folder_id: {r[4]}')
