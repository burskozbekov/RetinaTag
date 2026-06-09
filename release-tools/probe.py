import sqlite3, sys, os

db = sys.argv[1] if len(sys.argv) > 1 else os.path.expandvars(r'%TEMP%\retina_probe.db')
c = sqlite3.connect(db)
cur = c.cursor()

print('=== Watch folders ===')
try:
    for r in cur.execute('SELECT path, enabled, auto_tag FROM watch_folders LIMIT 20'):
        print(' ', r)
except Exception as e:
    print('  (no watch_folders table)', e)

print()
print('=== Distinct photo folders (top 30 by count) ===')
for r in cur.execute('SELECT folder, COUNT(*) FROM photos GROUP BY folder ORDER BY COUNT(*) DESC LIMIT 30'):
    print(f'  {r[1]:>6}  {r[0]}')

print()
print('=== XMP / metadata / embed settings ===')
for r in cur.execute("SELECT key, value FROM settings WHERE key LIKE '%xmp%' OR key LIKE '%metadata%' OR key LIKE '%sidecar%' OR key LIKE '%embed%' OR key LIKE '%auto%'"):
    print(' ', r)

print()
print('=== Sample photo paths from biggest folder ===')
top_folder_row = cur.execute('SELECT folder FROM photos GROUP BY folder ORDER BY COUNT(*) DESC LIMIT 1').fetchone()
if top_folder_row:
    top_folder = top_folder_row[0]
    print(f'  Top folder: {top_folder}')
    for r in cur.execute('SELECT path FROM photos WHERE folder=? LIMIT 5', (top_folder,)):
        print(f'  {r[0]}')
