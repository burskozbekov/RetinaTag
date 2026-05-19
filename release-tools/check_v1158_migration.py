import sqlite3
c = sqlite3.connect(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')
tbl = c.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='vault_folders'").fetchall()
print('vault_folders table:', tbl)
cols = [r[1] for r in c.execute('PRAGMA table_info(photos)').fetchall()]
print('vault_folder_id col present:', 'vault_folder_id' in cols)
idx = c.execute("SELECT name FROM sqlite_master WHERE type='index' AND name IN ('idx_vault_folders_parent','idx_photos_vault_folder')").fetchall()
print('indexes:', idx)
