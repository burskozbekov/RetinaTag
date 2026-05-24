"""
Count PNG files with NULL date_taken that have a Creation Time tEXt
chunk. These are recoverable — PC currently misses them.
"""
import sqlite3, sys, io, os, re
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

conn = sqlite3.connect(DB)

# Iterate all NULL PNGs; count those with Creation Time
total = 0
with_ctime = 0
samples = []
for r in conn.execute("SELECT path FROM photos WHERE date_taken IS NULL AND LOWER(filename) LIKE '%.png'"):
    p = r[0]
    if not os.path.exists(p): continue
    total += 1
    try:
        with open(p, 'rb') as f:
            head = f.read(128 * 1024)
        idx = head.find(b'tEXtCreation Time\x00')
        if idx < 0:
            idx = head.find(b'iTXtCreation Time\x00')
        if idx >= 0:
            with_ctime += 1
            # Try to extract the value
            start = idx + len(b'tEXtCreation Time\x00')
            end = start
            while end < len(head) and head[end] not in (0, 0xff) and end - start < 64:
                end += 1
            val = head[start:end].decode('ascii', errors='replace')
            if len(samples) < 8:
                samples.append((os.path.basename(p), val))
    except Exception:
        pass

print(f'NULL-date PNGs scanned: {total}')
print(f'Have Creation Time chunk: {with_ctime} ({100*with_ctime/max(total,1):.0f}%)')
print()
print('Sample (filename → value):')
for f, v in samples:
    print(f'  {f:<40} {v}')
conn.close()
