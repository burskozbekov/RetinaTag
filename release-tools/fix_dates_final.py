"""
v1.5.266 — Date pipeline canonical repair, third (and final) pass.

What changed since fix_dates_comprehensive.py (v1.5.263):
  1. HEIC support: register pillow_heif's HEIF opener so iPhone EXIF
     finally reads. The previous pass treated every HEIC as "no EXIF".
  2. Drop mtime/ctime as a candidate source. User: "Bu fotolar çok
     eski" — files freshly copied to PC were getting mtime ("today")
     as their date_taken, then floating to the top of "Newest →
     Oldest". mtime tells us when the file landed on the PC, not when
     the photo was taken.
  3. Path-pattern regex extended to also accept `\` and `/` as
     separators so a Windows folder like `\Fotograflar\2026\05-May\`
     contributes a YYYY-MM candidate. (Previously only -, _, ., :.)
  4. NULL is now a valid result. If no EXIF (after placeholder
     filtering) and no YYYY-MM(-DD) in the path → date_taken becomes
     NULL. The Gallery's "Newest → Oldest" treats NULL as the last
     bucket, which is where "no real date" photos belong until the
     user manually backfills them.

Run with the app closed.
"""
import sqlite3, sys, io, datetime, shutil, time, re, os
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

try:
    from PIL import Image
    from pillow_heif import register_heif_opener
    register_heif_opener()
except ImportError as e:
    print(f'Required: pip install Pillow pillow-heif  ({e})')
    sys.exit(1)

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

EXIF_DATE_TAGS = (0x9003, 0x9004, 0x0132)
EXIF_IFD_TAG   = 0x8769
IMAGE_EXTS = {'.jpg', '.jpeg', '.png', '.tif', '.tiff', '.heic', '.heif', '.webp'}

YEAR_MIN, YEAR_MAX = 1990, 2099
NOW   = datetime.datetime.now() + datetime.timedelta(days=1)
FLOOR = datetime.datetime(1990, 1, 1)

def is_placeholder(dt):
    if dt.hour != 0 or dt.minute != 0 or dt.second != 0: return False
    if dt.month != 1 or dt.day != 1: return False
    return dt.year in (1970, 1980, 2000, 2001, 2002)

def parse_exif_dt_str(raw):
    if isinstance(raw, bytes): raw = raw.decode('ascii', errors='ignore')
    raw = str(raw).strip()
    if len(raw) < 19: return None
    try:
        y = int(raw[0:4]); m = int(raw[5:7]); d = int(raw[8:10])
        hh = int(raw[11:13]); mm = int(raw[14:16]); ss = int(raw[17:19])
        return datetime.datetime(y, m, d, hh, mm, ss)
    except ValueError:
        return None

def gather_exif(path):
    out = []
    p = Path(path)
    if p.suffix.lower() not in IMAGE_EXTS: return out
    try:
        with Image.open(path) as img:
            exif = img.getexif()
            if exif:
                sources = [exif]
                try:
                    ifd = exif.get_ifd(EXIF_IFD_TAG)
                    if ifd: sources.append(ifd)
                except Exception:
                    pass
                for src in sources:
                    for tid in EXIF_DATE_TAGS:
                        if tid in src:
                            dt = parse_exif_dt_str(src[tid])
                            if dt and not is_placeholder(dt):
                                out.append((dt, 3))
    except Exception:
        pass
    # v1.5.268 — Also scan for EMBEDDED XMP in the JPEG's APP1
    # segment. Mac reads this; PIL's getexif() doesn't surface it.
    # FB exports / Photoshop output / phone cameras stash the real
    # capture date in xmp:CreateDate or photoshop:DateCreated here.
    try:
        if p.suffix.lower() in ('.jpg', '.jpeg', '.jpe'):
            out.extend(_gather_xmp(path))
    except Exception:
        pass
    return out

# v1.5.268 — Strip the JPEG, find the XMP APP1 segment, pull out
# xmp:CreateDate / photoshop:DateCreated / exif:DateTimeOriginal.
import struct, re
_RX_XMP_DATE = re.compile(
    rb'<(?:xmp:CreateDate|photoshop:DateCreated|exif:DateTimeOriginal)>'
    rb'(\d{4}[-:]\d{2}[-:]\d{2}[T ]\d{2}:\d{2}:\d{2})'
)
def _gather_xmp(path):
    out = []
    with open(path, 'rb') as f:
        head = f.read(4 * 1024 * 1024)  # XMP almost always in first 4 MB
    if head[:2] != b'\xff\xd8':
        return out
    i = 2
    xmp_xml = None
    while i < len(head) - 4 and head[i] == 0xff:
        marker = head[i+1]
        if marker in (0xd8, 0x00):
            i += 2; continue
        if marker == 0xda:  # SOS — image data starts; metadata done
            break
        seg_len = struct.unpack('>H', head[i+2:i+4])[0]
        body = head[i+4 : i+2+seg_len]
        if marker == 0xe1 and body.startswith(b'http://ns.adobe.com/xap/'):
            # Skip the namespace ID + its trailing NUL.
            nul = body.find(b'\x00')
            if nul >= 0:
                xmp_xml = body[nul+1:]
            break
        i += 2 + seg_len
    if not xmp_xml:
        return out
    for m in _RX_XMP_DATE.finditer(xmp_xml):
        raw = m.group(1).decode('ascii', errors='ignore')
        # raw matches "YYYY[-:]MM[-:]DD[T ]HH:MM:SS" — slice positions
        # are fixed regardless of which separator the producer chose.
        try:
            dt = datetime.datetime(
                int(raw[0:4]),  int(raw[5:7]),  int(raw[8:10]),
                int(raw[11:13]), int(raw[14:16]), int(raw[17:19]),
            )
            if not is_placeholder(dt):
                out.append((dt, 3))
        except (ValueError, IndexError):
            pass
    return out

# v1.5.266 — Path separators include \ and / so Windows + Unix paths
# both work. Previous regex only matched -, _, ., :.
RX_FULL = re.compile(r'(?:^|[^\d])((?:19|20)\d{2})[-_./:\\]?(\d{2})[-_./:\\]?(\d{2})(?:$|[^\d])')
RX_YM   = re.compile(r'(?:^|[^\d])((?:19|20)\d{2})[-_./:\\](\d{2})(?:$|[^\d])')

def gather_path(path):
    out = []
    seen_full = set()
    for m in RX_FULL.finditer(path):
        y, mo, d = int(m.group(1)), int(m.group(2)), int(m.group(3))
        if not (YEAR_MIN <= y <= YEAR_MAX): continue
        if not (1 <= mo <= 12): continue
        if not (1 <= d <= 31): continue
        try:
            out.append((datetime.datetime(y, mo, d, 12, 0, 0), 0))
            seen_full.add((y, mo))
        except ValueError:
            pass
    for m in RX_YM.finditer(path):
        y, mo = int(m.group(1)), int(m.group(2))
        if not (YEAR_MIN <= y <= YEAR_MAX): continue
        if not (1 <= mo <= 12): continue
        if (y, mo) in seen_full: continue
        try:
            out.append((datetime.datetime(y, mo, 1, 12, 0, 0), 0))
        except ValueError:
            pass
    return out

def _gather_mp4(path):
    """v1.5.272 — Pull moov/mvhd creation_time out of MP4/MOV/M4V."""
    p = Path(path)
    if p.suffix.lower() not in ('.mp4', '.mov', '.m4v', '.m4a', '.qt'):
        return []
    MAC_TO_UNIX = 2_082_844_800
    try:
        with open(path, 'rb') as f:
            file_len = p.stat().st_size
            # Find moov at top level.
            pos = 0
            moov = None  # (data_start, data_end)
            while pos + 8 <= file_len:
                f.seek(pos)
                head = f.read(8)
                if len(head) < 8: break
                size32 = int.from_bytes(head[:4], 'big')
                kind = head[4:8]
                hdr_extra = 0
                if size32 == 1:
                    ext = f.read(8)
                    atom_size = int.from_bytes(ext, 'big')
                    hdr_extra = 8
                elif size32 == 0:
                    atom_size = file_len - pos
                else:
                    atom_size = size32
                if atom_size < 8: break
                if kind == b'moov':
                    moov = (pos + 8 + hdr_extra, pos + atom_size)
                    break
                pos += atom_size
            if not moov: return []
            # Find mvhd inside moov.
            p2, end = moov
            while p2 + 8 <= end:
                f.seek(p2)
                head = f.read(8)
                if len(head) < 8: break
                size32 = int.from_bytes(head[:4], 'big')
                kind = head[4:8]
                hdr_extra = 0
                if size32 == 1:
                    ext = f.read(8)
                    atom_size = int.from_bytes(ext, 'big')
                    hdr_extra = 8
                elif size32 == 0:
                    atom_size = end - p2
                else:
                    atom_size = size32
                if atom_size < 8: break
                if kind == b'mvhd':
                    f.seek(p2 + 8 + hdr_extra)
                    ver_flags = f.read(4)
                    if len(ver_flags) < 4: return []
                    if ver_flags[0] == 1:
                        secs = int.from_bytes(f.read(8), 'big', signed=True)
                    else:
                        secs = int.from_bytes(f.read(4), 'big')
                    if secs == 0: return []
                    unix = secs - MAC_TO_UNIX
                    if not (0 <= unix <= 4_133_980_800): return []
                    dt = datetime.datetime.fromtimestamp(unix)
                    if is_placeholder(dt): return []
                    return [(dt, 3)]
                p2 += atom_size
    except Exception:
        pass
    return []

def best_date_for(path):
    cands = gather_exif(path) + gather_path(path) + _gather_mp4(path)
    cands = [(dt, q) for dt, q in cands if FLOOR <= dt <= NOW]
    if not cands:
        return None  # NULL
    # v1.5.266 — Pick by oldest YEAR-MONTH. A YYYY-MM path candidate
    # synthesizes day=1 noon and used to beat an EXIF date later in
    # the same month. Now: oldest (year, month) wins; within that
    # month, highest-quality time wins.
    oldest_ym = min((dt.year, dt.month) for dt, _ in cands)
    onMonth = [(dt, q) for dt, q in cands if (dt.year, dt.month) == oldest_ym]
    onMonth.sort(key=lambda t: (-t[1], t[0]))
    return onMonth[0][0].strftime('%Y-%m-%d %H:%M:%S')

ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
backup = Path(DB + f'.bak-final-dates-{ts}')
print(f'Backing up DB -> {backup.name}')
shutil.copy2(DB, backup)

conn = sqlite3.connect(DB, timeout=60)
conn.row_factory = sqlite3.Row
cur = conn.cursor()
rows = cur.execute("SELECT id, path, date_taken FROM photos").fetchall()
total = len(rows)
print(f'Total photos: {total:,}')

updated   = 0
nulled    = 0
unchanged = 0
missing   = 0
batch     = []
start     = time.time()

for i, r in enumerate(rows, 1):
    if i % 1000 == 0 or i == total:
        el = time.time() - start
        rate = i/el if el>0 else 0
        eta = (total-i)/rate if rate>0 else 0
        print(f'  {i:>6}/{total} upd={updated} null={nulled} unch={unchanged} miss={missing} eta={int(eta)}s')
        if batch:
            cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
            conn.commit()
            batch = []
    p = r['path']
    if not Path(p).exists():
        missing += 1
        continue
    canonical = best_date_for(p)  # may be None
    if canonical is None:
        if r['date_taken'] is None:
            unchanged += 1
        else:
            batch.append((None, r['id']))
            nulled += 1
        continue
    if canonical == r['date_taken']:
        unchanged += 1
        continue
    batch.append((canonical, r['id']))
    updated += 1

if batch:
    cur.executemany("UPDATE photos SET date_taken = ? WHERE id = ?", batch)
    conn.commit()

el = time.time() - start
print()
print(f'===== Done in {el:.1f}s =====')
print(f'Updated to real date: {updated:,}')
print(f'Set to NULL (no signal): {nulled:,}')
print(f'Unchanged:            {unchanged:,}')
print(f'Missing on disk:      {missing:,}')
print(f'Backup: {backup}')
conn.close()
