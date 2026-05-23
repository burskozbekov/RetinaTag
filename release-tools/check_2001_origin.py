"""
Where is `2001-01-01 00:00:00` coming from for these Instagram files?
EXIF is empty, mtime is 2026. Folder says 2019. Yet DB stores 2001-01-01.
Look at: XMP sidecars, IPTC, maybe an old snapshot.
"""
import sqlite3, sys, io, os, json
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

try:
    from PIL import Image
except ImportError:
    print('PIL needed'); sys.exit(1)

target = r'D:\Fotograflar\2019-04-18\AEVG1591.JPG'
print(f'File: {target}')
print(f'Exists: {os.path.exists(target)}')

# Look for XMP sidecar
for sx in ('.xmp', '.XMP'):
    side = os.path.splitext(target)[0] + sx
    if os.path.exists(side):
        print(f'\nXMP sidecar at: {side}')
        with open(side, 'rb') as f:
            data = f.read()
        # Look for 2001 in the XMP
        idx = data.find(b'2001')
        if idx >= 0:
            print(f'  found "2001" at offset {idx}:')
            print(f'  context: {data[max(0,idx-40):idx+60].decode("utf-8","replace")}')
        else:
            print('  XMP has no "2001" string')
            # Look for date-like patterns
            for tag in (b'<xmp:CreateDate', b'<xmp:ModifyDate', b'<photoshop:DateCreated',
                        b'<exif:DateTimeOriginal'):
                pos = data.find(tag)
                if pos >= 0:
                    print(f'  {tag.decode()}: {data[pos:pos+100].decode("utf-8","replace")}')
        break
else:
    print('No XMP sidecar found.')

# Look in JPEG APP1 (EXIF/XMP packet) for the literal 2001 substring.
print('\nScanning JPEG raw bytes for 2001:01:01 string …')
with open(target, 'rb') as f:
    data = f.read(2 * 1024 * 1024)  # first 2 MB
for needle in (b'2001:01:01', b'2001-01-01', b'2001\x3a01\x3a01'):
    pos = data.find(needle)
    if pos >= 0:
        print(f'  HIT {needle!r} at offset {pos}')
        print(f'  context: {data[max(0,pos-40):pos+60]!r}')
    else:
        print(f'  no hit for {needle!r}')

# Check DB row metadata
print('\nDB row:')
conn = sqlite3.connect(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db')
conn.row_factory = sqlite3.Row
# All columns
r = conn.execute("SELECT * FROM photos WHERE path = ?", (target,)).fetchone()
if r:
    for k in r.keys():
        v = r[k]
        if v is not None and (isinstance(v,str) and len(v) > 80):
            v = v[:80] + '…'
        print(f'  {k}: {v}')
conn.close()
