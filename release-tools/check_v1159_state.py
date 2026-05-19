import sqlite3
c = sqlite3.connect(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')
print('vault_folders count:', c.execute('SELECT COUNT(*) FROM vault_folders').fetchone()[0])
print('photos with vault_folder_id NOT NULL:', c.execute('SELECT COUNT(*) FROM photos WHERE vault_folder_id IS NOT NULL').fetchone()[0])
print('photos with private=1 (existing vault entries):', c.execute('SELECT COUNT(*) FROM photos WHERE private=1').fetchone()[0])
print('--- vault_folders sample (first 5) ---')
for r in c.execute('SELECT id,name,parent_id,original_path FROM vault_folders LIMIT 5'):
    print(r)
