import os, sys
sys.stdout.reconfigure(encoding='utf-8')

total = 0
no_photo_match = 0
import sqlite3
db = os.path.expandvars(r'%TEMP%\retina_probe\retina.db')
c = sqlite3.connect(db)
cur = c.cursor()

# Build set of photo stems for quick lookup
print('Loading photo paths from DB...')
photo_stems = set()
for path, in cur.execute("SELECT path FROM photos"):
    p = path.replace('/', '\\').lower()
    # Lightroom-style sidecar: stem.xmp (no ext)
    base = os.path.splitext(p)[0]
    photo_stems.add(base + '.xmp')
    # DigiKam style: full-name.xmp
    photo_stems.add(p + '.xmp')

print(f'Loaded {len(photo_stems)} possible sidecar paths from DB.')
print()
print('Walking D:\\Fotograflar for .xmp files...')

for root, dirs, files in os.walk(r'D:\Fotograflar'):
    for fn in files:
        if fn.lower().endswith('.xmp'):
            total += 1
            full = os.path.join(root, fn).lower()
            if full not in photo_stems:
                no_photo_match += 1

print(f'Total .xmp on disk:                   {total:,}')
print(f'XMPs WITH a photo row in DB:          {total - no_photo_match:,}')
print(f'XMPs WITHOUT matching photo (orphan): {no_photo_match:,}')
