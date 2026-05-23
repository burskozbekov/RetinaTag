"""
v1.5.272 — Dry-run: find every photo whose CURRENT FOLDER claims
year-month X but whose date_taken says year-month Y, when the folder
matches the MTP-import-style pattern \YYYY\MM-MonthName\.

User-organized folders (\DUZENLE\, \samsung\, \Portrait\, \superturk\,
year-only \2019-04-18\) are LEFT ALONE. We only touch the
mechanically-generated MTP buckets that we (or Mac) created.

This script ONLY reports. The actual move/update happens in
rebucket_apply.py.
"""
import sqlite3, io, sys, re
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

# Folder pattern produced by MTP import (Mac v1.5.147 + PC v1.5.147+):
#   ...\<library_root>\<YYYY>\<MM>-<MonthName>\<filename>
MONTHS = ('January','February','March','April','May','June',
          'July','August','September','October','November','December')
PAT = re.compile(
    r'^(.*?)[\\/](19\d{2}|20\d{2})[\\/](0[1-9]|1[0-2])-('
    + '|'.join(MONTHS) + r')[\\/]([^\\/]+)$',
    re.I,
)

conn = sqlite3.connect(DB)
conn.row_factory = sqlite3.Row

matched = 0
mismatched = []
for r in conn.execute("SELECT id, path, date_taken FROM photos WHERE date_taken IS NOT NULL").fetchall():
    m = PAT.match(r['path'])
    if not m: continue
    root, fy_s, fm_s, fmonth, fn = m.groups()
    fy, fm = int(fy_s), int(fm_s)
    dy = int(r['date_taken'][0:4])
    dm = int(r['date_taken'][5:7])
    if (fy, fm) == (dy, dm):
        matched += 1
        continue
    mismatched.append({
        'id': r['id'],
        'path': r['path'],
        'root': root,
        'cur_y': fy, 'cur_m': fm,
        'real_y': dy, 'real_m': dm,
        'filename': fn,
    })

print(f'Photos in MTP-style buckets:')
print(f'  correct folder: {matched:,}')
print(f'  WRONG folder:   {len(mismatched):,}')
print()
print('Sample (first 30):')
for x in mismatched[:30]:
    print(f"  id={x['id']:>5}  {x['cur_y']}/{x['cur_m']:02d} -> {x['real_y']}/{x['real_m']:02d}  {x['filename'][:40]}")
print()
print('Sources (where they currently sit):')
from collections import Counter
src_cnt = Counter((x['cur_y'], x['cur_m']) for x in mismatched)
for (y, m), c in src_cnt.most_common(15):
    print(f"  {y}/{m:02d}: {c:,} files")
print()
print('Destinations (where they should go):')
dst_cnt = Counter((x['real_y'], x['real_m']) for x in mismatched)
for (y, m), c in dst_cnt.most_common(15):
    print(f"  {y}/{m:02d}: {c:,} files")

conn.close()
