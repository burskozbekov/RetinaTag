"""
Find Apple QuickTime metadata atoms in MP4/MOV files where mvhd is
zero. Apple writes capture date as moov/udta/©day or
moov/meta/com.apple.quicktime.creationdate — AVFoundation reads
both. PC currently misses both.

Also: check sample JPG with empty DateTimeOriginal — what date tags
DO exist?
"""
import sqlite3, sys, io, os, struct
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

def walk_atoms(data, start, end, depth=0, limit=4):
    """Iterate (offset, kind, data_start, data_end) over atoms in [start, end)."""
    p = start
    while p + 8 <= end and depth < limit:
        sz = int.from_bytes(data[p:p+4], 'big')
        kind = data[p+4:p+8]
        he = 0
        if sz == 1:
            sz = int.from_bytes(data[p+8:p+16], 'big'); he = 8
        elif sz == 0:
            sz = end - p
        if sz < 8: break
        yield (p, kind, p + 8 + he, p + sz, depth)
        p += sz

def find_path(data, atom_path):
    """Walk down a path like [b'moov', b'udta', b'©day']."""
    cur_start = 0
    cur_end = len(data)
    for needle in atom_path:
        found = None
        for off, kind, ds, de, _ in walk_atoms(data, cur_start, cur_end, depth=0, limit=1):
            if kind == needle:
                found = (ds, de); break
        if not found: return None
        cur_start, cur_end = found
    return (cur_start, cur_end)

print('=== MP4/MOV files with mvhd=0 — what other date atoms exist? ===')
conn = sqlite3.connect(DB)
checked = 0
found_apple = 0
for r in conn.execute("SELECT path FROM photos WHERE date_taken IS NULL AND (LOWER(filename) LIKE '%.mp4' OR LOWER(filename) LIKE '%.mov') ORDER BY RANDOM() LIMIT 12"):
    p = r[0]
    if not os.path.exists(p): continue
    checked += 1
    try:
        # Read enough to walk moov (usually < 2 MB).
        with open(p, 'rb') as f:
            data = f.read(8 * 1024 * 1024)
        # Look for moov/udta/©day  (©day = 0xa9 'd' 'a' 'y')
        cday = b'\xa9day'
        cdat = b'\xa9dtm'  # alternative
        # Look for keys/data pair in moov/meta with creationdate key.
        creation_str = b'com.apple.quicktime.creationdate'
        idx_cday = data.find(cday)
        idx_str  = data.find(creation_str)
        labels = []
        if idx_cday >= 0:
            # ©day atom: size(4) + ©day + data — for ©day it's [size][©day][data_size][...] usually
            # Real format: ©day atom contains a "data" subatom with the string
            # Look at the next 32 bytes
            chunk = data[idx_cday-4:idx_cday+64]
            labels.append(f'©day @{idx_cday} chunk={chunk[:40]!r}')
        if idx_str >= 0:
            # Find the matching data — keys/items structure
            # Just dump nearby bytes
            chunk = data[idx_str:idx_str+200]
            # Look for date-like ascii nearby (YYYY-MM-DDTHH...)
            import re
            m = re.search(rb'(\d{4}[-:]\d{2}[-:]\d{2}[T ]\d{2}:\d{2}:\d{2})', chunk)
            dt_str = m.group(1).decode('ascii','replace') if m else 'none'
            labels.append(f'com.apple.quicktime.creationdate present, nearby date: {dt_str}')
        if labels:
            found_apple += 1
            print(f'  {os.path.basename(p)}:')
            for l in labels:
                print(f'    {l}')
        else:
            print(f'  {os.path.basename(p)}: no Apple atoms')
    except Exception as e:
        print(f'  {p}: {e}')

print()
print(f'Apple atoms found in {found_apple}/{checked} samples')

print()
print('=== JPG with EXIF but NULL date — full tag dump ===')
try:
    from PIL import Image
    from PIL.ExifTags import TAGS
    for r in conn.execute("SELECT path FROM photos WHERE date_taken IS NULL AND LOWER(filename) LIKE '%.jpg' ORDER BY RANDOM() LIMIT 8"):
        p = r[0]
        if not os.path.exists(p): continue
        try:
            with Image.open(p) as img:
                exif = img.getexif()
                ifd_exif = exif.get_ifd(0x8769) if exif else {}
                ifd_gps = exif.get_ifd(0x8825) if exif else {}
                date_tags = {}
                # Look for ANY tag with "Date" or "Time" in its name across all dicts
                for src_name, src in [('IFD0', exif), ('EXIF', ifd_exif), ('GPS', ifd_gps)]:
                    if not src: continue
                    for tid, val in src.items():
                        name = TAGS.get(tid, hex(tid) if isinstance(tid, int) else str(tid))
                        if 'Date' in name or 'Time' in name:
                            date_tags[f'{src_name}.{name}'] = repr(val)[:60]
                if date_tags:
                    print(f'  {os.path.basename(p):<30}')
                    for k, v in date_tags.items():
                        print(f'    {k}: {v}')
                else:
                    sz = exif and len(dict(exif))
                    print(f'  {os.path.basename(p):<30} no date tags (total exif keys={sz})')
        except Exception as e:
            print(f'  {p}: {e}')
except ImportError:
    print('PIL not available')

conn.close()
