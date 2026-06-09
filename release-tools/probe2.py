import sqlite3, sys, os, glob
sys.stdout.reconfigure(encoding='utf-8')

db = os.path.expandvars(r'%TEMP%\retina_probe.db')
c = sqlite3.connect(db)
cur = c.cursor()

print('=== XMP / metadata / auto settings ===')
for r in cur.execute("SELECT key, value FROM settings WHERE key LIKE '%xmp%' OR key LIKE '%metadata%' OR key LIKE '%sidecar%' OR key LIKE '%embed%' OR key LIKE '%auto%' OR key LIKE '%import%'"):
    print(f'  {r[0]} = {r[1]}')

print()
print('=== Probe shared folder for .xmp files ===')
folders_to_check = [
    r'D:\Fotograflar',
    r'D:\Fotograflar\2024-10-09',
    r'D:\Fotograflar\2024-2025 SEP',
    r'D:\Fotograflar\DUZENLE',
]
for f in folders_to_check:
    if not os.path.isdir(f):
        print(f'  MISSING: {f}')
        continue
    # Recursive .xmp glob, capped at 5
    matches = []
    for root, dirs, files in os.walk(f):
        for fn in files:
            if fn.lower().endswith('.xmp'):
                matches.append(os.path.join(root, fn))
                if len(matches) >= 5:
                    break
        if len(matches) >= 5:
            break
    print(f'  {f}')
    if matches:
        print(f'    Found {len(matches)} .xmp file(s):')
        for m in matches:
            sz = os.path.getsize(m)
            print(f'      {sz:>6} bytes  {m}')
    else:
        print(f'    NO .xmp files found')
print()

print('=== Sample tagged photos (last 10 tagged) ===')
for r in cur.execute("SELECT p.path, COUNT(t.id) FROM photos p LEFT JOIN tags t ON p.id=t.photo_id GROUP BY p.id HAVING COUNT(t.id) > 0 ORDER BY p.id DESC LIMIT 10"):
    print(f'  tags={r[1]:>3}  {r[0]}')

print()
print('=== Sample UNTAGGED photos ===')
for r in cur.execute("SELECT p.path FROM photos p LEFT JOIN tags t ON p.id=t.photo_id WHERE t.id IS NULL LIMIT 5"):
    print(f'  {r[0]}')
