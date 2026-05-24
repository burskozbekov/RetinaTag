"""
Forensic: pick samples of NULL-date_taken files per extension,
inspect their actual content, find what metadata path PC is missing.
"""
import sqlite3, sys, io, os, struct, re
from pathlib import Path
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

DB = r'C:\Users\dede_\AppData\Roaming\com.retinatag.app\retina.db'

# Inline MP4 mvhd reader (same algorithm as our scanner).
def mp4_creation(path):
    MAC_TO_UNIX = 2_082_844_800
    try:
        with open(path, 'rb') as f:
            file_len = os.path.getsize(path)
            pos = 0
            moov = None
            while pos + 8 <= file_len:
                f.seek(pos)
                h = f.read(8)
                if len(h) < 8: break
                sz = int.from_bytes(h[:4], 'big')
                kind = h[4:8]
                he = 0
                if sz == 1:
                    ext = f.read(8); sz = int.from_bytes(ext, 'big'); he = 8
                elif sz == 0:
                    sz = file_len - pos
                if sz < 8: break
                if kind == b'moov':
                    moov = (pos + 8 + he, pos + sz); break
                pos += sz
            if not moov: return ('no moov', None)
            p2, end = moov
            while p2 + 8 <= end:
                f.seek(p2)
                h = f.read(8)
                if len(h) < 8: break
                sz = int.from_bytes(h[:4], 'big'); kind = h[4:8]; he = 0
                if sz == 1: ext = f.read(8); sz = int.from_bytes(ext,'big'); he = 8
                elif sz == 0: sz = end - p2
                if sz < 8: break
                if kind == b'mvhd':
                    f.seek(p2 + 8 + he)
                    vf = f.read(4)
                    if len(vf) < 4: return ('mvhd short', None)
                    if vf[0] == 1:
                        secs = int.from_bytes(f.read(8), 'big', signed=True)
                    else:
                        secs = int.from_bytes(f.read(4), 'big')
                    if secs == 0: return ('mvhd zero', None)
                    return ('mvhd ok', secs - MAC_TO_UNIX)
                p2 += sz
            return ('no mvhd', None)
    except Exception as e:
        return (f'err {e}', None)

def first_200_atoms(path):
    """Return list of (offset, atom_type) from top-level atoms."""
    out = []
    try:
        with open(path, 'rb') as f:
            file_len = os.path.getsize(path)
            pos = 0
            while pos + 8 <= file_len and len(out) < 20:
                f.seek(pos)
                h = f.read(8)
                if len(h) < 8: break
                sz = int.from_bytes(h[:4], 'big')
                kind = h[4:8]
                if sz == 0: sz = file_len - pos
                elif sz == 1:
                    ext = f.read(8)
                    sz = int.from_bytes(ext, 'big')
                out.append((pos, kind.decode('ascii','replace'), sz))
                if sz < 8: break
                pos += sz
    except Exception:
        pass
    return out

conn = sqlite3.connect(DB)

print('=== MP4 NULL — what does mvhd reader say? ===')
for r in conn.execute("SELECT path FROM photos WHERE date_taken IS NULL AND LOWER(filename) LIKE '%.mp4' ORDER BY RANDOM() LIMIT 5"):
    p = r[0]
    if not os.path.exists(p):
        print(f'  MISSING {p}'); continue
    status, secs = mp4_creation(p)
    import datetime as dt
    when = dt.datetime.fromtimestamp(secs).strftime('%Y-%m-%d %H:%M:%S') if secs and 0<secs<4133980800 else 'n/a'
    print(f'  {os.path.basename(p):<30} mvhd={status} when={when}')
    if status == 'no moov':
        atoms = first_200_atoms(p)
        print(f'     top atoms: {[a[1] for a in atoms[:6]]}')
print()

print('=== PNG NULL — what time chunks? ===')
for r in conn.execute("SELECT path FROM photos WHERE date_taken IS NULL AND LOWER(filename) LIKE '%.png' ORDER BY RANDOM() LIMIT 5"):
    p = r[0]
    if not os.path.exists(p):
        print(f'  MISSING {p}'); continue
    try:
        with open(p, 'rb') as f:
            data = f.read(64 * 1024)
        found = []
        for tag in (b'tIME', b'tEXt', b'iTXt', b'eXIf'):
            if tag in data:
                idx = data.find(tag)
                snippet = data[idx:idx+80]
                found.append((tag.decode(), snippet[:60]))
        label = [t[0] for t in found] if found else 'none'
        print(f'  {os.path.basename(p):<30} chunks={label}')
        for t, s in found[:1]:
            print(f'     {t}: {s!r}')
    except Exception as e:
        print(f'  {p}: {e}')
print()

print('=== JPG NULL — what does APP1/IPTC look like? ===')
for r in conn.execute("SELECT path FROM photos WHERE date_taken IS NULL AND LOWER(filename) LIKE '%.jpg' ORDER BY RANDOM() LIMIT 5"):
    p = r[0]
    if not os.path.exists(p): continue
    try:
        with open(p, 'rb') as f: data = f.read(4*1024*1024)
        if data[:2] != b'\xff\xd8':
            print(f'  {os.path.basename(p):<30} not JPEG?')
            continue
        # Find marker zoo
        markers = []
        i = 2
        while i < min(len(data) - 4, 128*1024):
            if data[i] != 0xff: break
            m = data[i+1]
            if m in (0xd8, 0x00, 0xd9): i += 2; continue
            if m == 0xda: break  # SOS
            seg_len = struct.unpack('>H', data[i+2:i+4])[0]
            body = data[i+4 : i+2+seg_len]
            tag = f'APP{m & 0xf}' if 0xe0 <= m <= 0xef else f'M{m:02x}'
            head = body[:20].decode('ascii','replace')
            markers.append((tag, head, seg_len))
            i += 2 + seg_len
        # Are there XMP / IPTC / Photoshop / EXIF segments?
        relevant = [m for m in markers if any(k in m[1] for k in ('Exif', 'http://', 'Photoshop', 'XMP', 'Adobe'))]
        print(f'  {os.path.basename(p):<30} segs={len(markers)}  relevant={len(relevant)}')
        for tag, head, sz in relevant[:3]:
            print(f'     {tag}({sz}b): {head!r}')
        # Search raw for year strings
        years = set(re.findall(rb'(19[89]\d|20[012]\d)', data[:64*1024]))
        if years:
            print(f'     year tokens in head: {sorted(y.decode() for y in years)[:5]}')
    except Exception as e:
        print(f'  {p}: {e}')
print()

print('=== WebP NULL — Exif chunks? ===')
for r in conn.execute("SELECT path FROM photos WHERE date_taken IS NULL AND LOWER(filename) LIKE '%.webp' ORDER BY RANDOM() LIMIT 5"):
    p = r[0]
    if not os.path.exists(p): continue
    try:
        with open(p, 'rb') as f: data = f.read(64*1024)
        if data[:4] != b'RIFF' or data[8:12] != b'WEBP':
            print(f'  {os.path.basename(p):<30} not WebP?')
            continue
        # Walk chunks
        chunks = []
        i = 12
        while i + 8 <= len(data):
            cid = data[i:i+4].decode('ascii','replace')
            sz = struct.unpack('<I', data[i+4:i+8])[0]
            chunks.append((cid, sz))
            i += 8 + sz + (sz & 1)
        print(f'  {os.path.basename(p):<30} chunks={[c[0] for c in chunks[:8]]}')
    except Exception as e:
        print(f'  {p}: {e}')

conn.close()
