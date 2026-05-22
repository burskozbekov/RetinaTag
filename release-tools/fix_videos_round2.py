"""
v1.5.239 — Round 2 of the video-thumb fix.

Two things in one pass:

1. Re-prune orphan DB rows that appeared after v1.5.237's first
   cleanup (14 new ones — likely from another import that hit a
   different month bucket before the file moved). Same rule as
   before: only delete the orphan if a sibling row with the same
   filename has a live file, so we never lose media.

2. Regenerate the ~6,637 video thumbnails whose thumbnail_path
   is set in the DB but the .jpg cache file is missing on disk.
   Uses ffmpeg directly (already installed at v6.0). Seeks to
   1 second in, dumps a 320×ANY scaled JPEG into the path the
   DB expects. The app reads that path verbatim so nothing else
   needs to change.

Run with the app CLOSED so the DB isn't locked.
"""
import sqlite3, sys, io, datetime, shutil, subprocess, time
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-videos-r2-{ts}')
print(f'Backing up DB → {backup.name}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=30)
conn.row_factory = sqlite3.Row
cur = conn.cursor()

# ── Pass 1: orphan DB rows ────────────────────────────────────────
print()
print('=== PASS 1: orphan DB rows ===')
all_rows = cur.execute(
    "SELECT id, path, filename FROM photos"
).fetchall()
print(f'Total photos rows: {len(all_rows):,}')

live_by_name = {}
orphans = []
for r in all_rows:
    if Path(r['path']).exists():
        live_by_name.setdefault(r['filename'], []).append(r['id'])
    else:
        orphans.append((r['id'], r['path'], r['filename']))

print(f'Orphans (file missing): {len(orphans)}')
to_drop = [(rid, p, fn) for (rid, p, fn) in orphans if fn in live_by_name]
no_sibling = [(rid, p, fn) for (rid, p, fn) in orphans if fn not in live_by_name]
print(f'  …with a live sibling → delete: {len(to_drop)}')
print(f'  …genuinely missing → preserve: {len(no_sibling)}')

if to_drop:
    ids_csv = ','.join(str(t[0]) for t in to_drop)
    for table, col in (('tags', 'photo_id'), ('face_regions', 'photo_id'), ('mtp_imports', 'photo_id')):
        try:
            n = cur.execute(f"DELETE FROM {table} WHERE {col} IN ({ids_csv})").rowcount
            if n: print(f'  cascaded {table}: {n} rows')
        except sqlite3.OperationalError:
            pass
    n = cur.execute(f"DELETE FROM photos WHERE id IN ({ids_csv})").rowcount
    print(f'  photos rows deleted: {n}')
    conn.commit()

if no_sibling:
    print(f'  Preserved (no sibling):')
    for rid, p, fn in no_sibling[:10]:
        print(f'    id={rid} {fn} -> {p}')

# ── Pass 2: regenerate missing video thumbnails ──────────────────
print()
print('=== PASS 2: regenerate missing video thumbnails ===')
missing_thumbs = cur.execute(
    "SELECT id, path, thumbnail_path FROM photos "
    "WHERE media_type = 'video' "
    "  AND thumbnail_path IS NOT NULL AND thumbnail_path != ''"
).fetchall()

# Filter: video exists on disk AND its thumbnail file does NOT
todo = []
for r in missing_thumbs:
    if Path(r['path']).exists() and not Path(r['thumbnail_path']).exists():
        todo.append((r['id'], r['path'], r['thumbnail_path']))
print(f'Videos needing thumb regen: {len(todo):,}')
if not todo:
    print('Nothing to regen. Done.')
    conn.close()
    sys.exit(0)

# Make sure the thumbnails dir exists
thumbs_dir = Path(todo[0][2]).parent
thumbs_dir.mkdir(parents=True, exist_ok=True)

ok = 0
fail = 0
fail_examples = []
start = time.time()
for i, (rid, video_path, thumb_path) in enumerate(todo, 1):
    if i % 100 == 0 or i == len(todo):
        elapsed = time.time() - start
        rate = i / elapsed if elapsed > 0 else 0
        eta = (len(todo) - i) / rate if rate > 0 else 0
        print(f'  {i:>6} / {len(todo)}  ok={ok}  fail={fail}  rate={rate:.1f}/s  eta={int(eta)}s')
    # Skip if somehow already there now (concurrent run, retry)
    if Path(thumb_path).exists():
        ok += 1
        continue
    # ffmpeg: seek to 1s, grab one frame, scale to 320 width preserving
    # AR, encode as JPEG q=4 (visually clean, small file).
    args = [
        'ffmpeg', '-ss', '1', '-i', video_path,
        '-frames:v', '1',
        '-vf', 'scale=320:-1',
        '-q:v', '4',
        '-y', '-loglevel', 'error',
        thumb_path
    ]
    try:
        proc = subprocess.run(args, capture_output=True, timeout=20)
        if proc.returncode == 0 and Path(thumb_path).exists() and Path(thumb_path).stat().st_size > 0:
            ok += 1
        else:
            fail += 1
            if len(fail_examples) < 6:
                err = proc.stderr.decode('utf-8', errors='replace').strip()[:140]
                fail_examples.append((video_path, err))
    except subprocess.TimeoutExpired:
        fail += 1
        if len(fail_examples) < 6:
            fail_examples.append((video_path, 'TIMEOUT'))
    except Exception as e:
        fail += 1
        if len(fail_examples) < 6:
            fail_examples.append((video_path, str(e)[:140]))

elapsed = time.time() - start
print()
print(f'===== Pass 2 done in {elapsed:.1f}s =====')
print(f'Regenerated: {ok:,}')
print(f'Failed:      {fail:,}')
if fail_examples:
    print(f'Sample failures:')
    for p, err in fail_examples:
        print(f'  {Path(p).name}: {err}')

conn.close()
print()
print(f'DB backup: {backup}')
