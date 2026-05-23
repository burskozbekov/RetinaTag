"""
Deep dive on the 12:12:12 timestamp cluster. We want to know:
- How many total photos
- File extensions (HEIC? MP4? JPG?)
- Whether the EXIF actually contains 12:12:12 or if it's synthesized somewhere downstream
- What mtime says for them
- What other date sources we could use
"""
import sqlite3, sys, io, os
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
conn = sqlite3.connect(DB, timeout=30)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

n = cur.execute("SELECT COUNT(*) FROM photos WHERE date_taken LIKE '% 12:12:12'").fetchone()[0]
print(f'Total photos with 12:12:12 timestamp: {n}')

# Extension breakdown
print('\nBy extension:')
for r in cur.execute("""
    SELECT LOWER(SUBSTR(filename, INSTR(filename,'.')+1)) AS ext, COUNT(*) c
    FROM photos
    WHERE date_taken LIKE '% 12:12:12'
    GROUP BY ext
    ORDER BY c DESC
""").fetchall():
    print(f'  .{r["ext"]:<6}: {r["c"]}')

# Folder breakdown - top 10
print('\nTop 10 folders containing 12:12:12 photos:')
for r in cur.execute("""
    SELECT
        SUBSTR(path, 1, LENGTH(path) - LENGTH(filename) - 1) AS folder,
        COUNT(*) c
    FROM photos
    WHERE date_taken LIKE '% 12:12:12'
    GROUP BY folder
    ORDER BY c DESC
    LIMIT 10
""").fetchall():
    print(f'  {r["c"]:>4} → {r["folder"]}')

# Try to read EXIF DateTimeOriginal directly to verify the SOURCE has 12:12:12
# (vs. our code synthesizing it).
print('\nReading actual EXIF for 5 sample 12:12:12 photos:')
try:
    from PIL import Image
    from PIL.ExifTags import TAGS
    samples = cur.execute("SELECT id, path FROM photos WHERE date_taken LIKE '% 12:12:12' ORDER BY RANDOM() LIMIT 5").fetchall()
    for r in samples:
        path = r['path']
        if not os.path.exists(path):
            print(f'  id={r["id"]}: FILE MISSING → {path[:80]}')
            continue
        try:
            ext = os.path.splitext(path)[1].lower()
            if ext == '.heic':
                print(f'  id={r["id"]} .heic — PIL may not read; skipping')
                continue
            img = Image.open(path)
            exif = img._getexif() or {}
            named = {TAGS.get(k,k):v for k,v in exif.items()}
            dto = named.get('DateTimeOriginal')
            dt  = named.get('DateTime')
            dtd = named.get('DateTimeDigitized')
            mtime = os.path.getmtime(path)
            from datetime import datetime
            mts = datetime.fromtimestamp(mtime).strftime('%Y-%m-%d %H:%M:%S')
            print(f'  id={r["id"]}: DTO={dto} DT={dt} DTD={dtd} mtime={mts}')
        except Exception as e:
            print(f'  id={r["id"]}: ERR {type(e).__name__}: {str(e)[:80]}')
except ImportError:
    print('  PIL not available — skipping EXIF read')

# Most-common 12:12:12 dates (date part):
print('\nTop 15 dates among 12:12:12 cluster:')
for r in cur.execute("""
    SELECT SUBSTR(date_taken,1,10) AS d, COUNT(*) c
    FROM photos
    WHERE date_taken LIKE '% 12:12:12'
    GROUP BY d
    ORDER BY c DESC
    LIMIT 15
""").fetchall():
    print(f'  {r["d"]}: {r["c"]}')

conn.close()
