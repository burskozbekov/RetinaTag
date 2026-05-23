"""
Check the 15 photos stamped 2001-01-01 00:00:00.
Are these legit (camera was actually broken in 2001 and user kept time wrong)
or is the EXIF a known camera default that should be ignored?
"""
import sqlite3, sys, io, os
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

try:
    from PIL import Image
    from PIL.ExifTags import TAGS
except ImportError:
    print('PIL needed'); sys.exit(1)

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
conn = sqlite3.connect(DB)
conn.row_factory = sqlite3.Row
rows = conn.execute("SELECT id, path FROM photos WHERE date_taken LIKE '2001-01-01%'").fetchall()
for r in rows:
    p = r['path']
    if not os.path.exists(p):
        print(f'  id={r["id"]} MISSING {p}')
        continue
    try:
        img = Image.open(p)
        exif = img._getexif() or {}
        named = {TAGS.get(k,k):v for k,v in exif.items()}
        dto  = named.get('DateTimeOriginal','')
        dt   = named.get('DateTime','')
        make = named.get('Make','')
        model= named.get('Model','')
        sw   = named.get('Software','')
        st = os.stat(p)
        import datetime
        mtime = datetime.datetime.fromtimestamp(st.st_mtime).strftime('%Y-%m-%d %H:%M:%S')
        ctime = datetime.datetime.fromtimestamp(st.st_ctime).strftime('%Y-%m-%d %H:%M:%S')
        print(f'  id={r["id"]}')
        print(f'    file: {p}')
        print(f'    EXIF DTO  : {dto}')
        print(f'    EXIF DT   : {dt}')
        print(f'    Make/Model: {make!r}/{model!r}  SW: {sw!r}')
        print(f'    mtime/ctime: {mtime} / {ctime}')
    except Exception as e:
        print(f'  id={r["id"]} ERR {e}')
