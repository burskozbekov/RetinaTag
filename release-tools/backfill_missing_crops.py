"""
v1.5.227 — For the 6 persons whose face_<id>.jpg crops were missing
on disk, crop the face_region bbox right out of the photo and save
it as face_<id>.jpg so the avatar can render.
"""
import sqlite3, sys, io
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

try:
    from PIL import Image
except ImportError:
    print("Pillow required")
    sys.exit(1)

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'
FACES_DIR = Path(r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\thumbnails\faces')

conn = sqlite3.connect(DB, timeout=30)
cur = conn.cursor()

# Same "needy" list as backfill — persons with no thumbnail
needy = cur.execute(
    "SELECT id, name FROM persons WHERE thumbnail IS NULL OR thumbnail = ''"
).fetchall()
print(f'Persons still missing thumbnail: {len(needy)}')

# Indexable set of disk face_<id>.jpg files
disk_face_ids = set()
for f in FACES_DIR.iterdir():
    if f.suffix.lower() == '.jpg' and f.stem.startswith('face_'):
        try:
            disk_face_ids.add(int(f.stem.split('_', 1)[1]))
        except ValueError:
            pass

cropped = 0
failed = 0
for pid, name in needy:
    # Best face_region for this person — highest score
    rows = cur.execute(
        """SELECT fr.id, fr.x1, fr.y1, fr.x2, fr.y2, fr.score, p.path
             FROM face_regions fr
             JOIN photos p ON fr.photo_id = p.id
            WHERE fr.person_id = ?
            ORDER BY fr.score DESC""",
        (pid,)
    ).fetchall()
    if not rows:
        print(f'  {name!r}: no face_regions at all — skipped')
        failed += 1
        continue

    success = False
    for face_id, x1, y1, x2, y2, score, photo_path in rows:
        if face_id in disk_face_ids:
            # Crop already exists, just patch DB
            cur.execute("UPDATE persons SET thumbnail = ? WHERE id = ?",
                        (f'face_{face_id}.jpg', pid))
            cropped += 1
            success = True
            print(f'  {name!r}: already had crop face_{face_id}.jpg, DB patched')
            break
        # Crop from the photo
        p = Path(photo_path)
        if not p.exists():
            continue
        try:
            with Image.open(p) as img:
                img = img.convert('RGB')
                iw, ih = img.size
                pad = max((x2 - x1), (y2 - y1)) // 5
                pad = max(pad, 8)
                cx1 = max(0, x1 - pad)
                cy1 = max(0, y1 - pad)
                cx2 = min(iw, x2 + pad)
                cy2 = min(ih, y2 + pad)
                crop = img.crop((cx1, cy1, cx2, cy2)).resize((128, 128), Image.LANCZOS)
                out_path = FACES_DIR / f'face_{face_id}.jpg'
                crop.save(out_path, 'JPEG', quality=85)
                cur.execute("UPDATE persons SET thumbnail = ? WHERE id = ?",
                            (f'face_{face_id}.jpg', pid))
                cropped += 1
                success = True
                print(f'  {name!r}: cropped face_{face_id}.jpg from {p.name}')
                break
        except Exception as e:
            print(f'  {name!r}: crop failed on {p.name}: {e}')
            continue

    if not success:
        failed += 1
        print(f'  {name!r}: no usable photo found, leaving NULL')

conn.commit()

after = cur.execute(
    "SELECT COUNT(*) FROM persons WHERE thumbnail IS NOT NULL AND thumbnail != ''"
).fetchone()[0]
total = cur.execute("SELECT COUNT(*) FROM persons").fetchone()[0]

print()
print('===== Done =====')
print(f'Cropped/patched: {cropped}')
print(f'Failed:          {failed}')
print(f'Coverage:        {after} / {total}')

conn.close()
