import sqlite3, os
c = sqlite3.connect(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')

print('=== vault_folders rows ===')
rows = c.execute('SELECT id, name, parent_id, original_path FROM vault_folders').fetchall()
for r in rows: print(r)
print(f'total: {len(rows)}')

print()
print('=== private photos: path + folder_id ===')
for r in c.execute("""
  SELECT id, filename, path, vault_folder_id FROM photos WHERE private=1
""").fetchall():
    in_store = 'vault-store' in r[2]
    print(f'id={r[0]} fn={r[1][:25]} store={in_store} folder_id={r[3]}')

print()
vault_dir = r'C:\Users\dede_\Desktop\VAULT'
print(f'Desktop\\VAULT exists: {os.path.exists(vault_dir)}')
if os.path.exists(vault_dir):
    print(f'  contents: {os.listdir(vault_dir)}')

store_dir = r'C:\Users\dede_\AppData\Local\com.retinatag.app\vault-store'
print(f'vault-store exists: {os.path.exists(store_dir)}')
if os.path.exists(store_dir):
    files = os.listdir(store_dir)
    print(f'  {len(files)} files: {files[:3]}{"..." if len(files) > 3 else ""}')
