import sqlite3, urllib.request, os, sys
db = r'D:\Fotograflar\RetinaTag\retina.db'
conn = sqlite3.connect(db)
row = conn.execute("SELECT token, addr, port FROM lan_peer_tokens LIMIT 1").fetchone()
token, addr, port = row
print(f"Peer: {addr}:{port}")

# Get first vault photo id from Mac
url = f'http://{addr}:{port}/api/photos?vault_only=true&offset=0&limit=1'
req = urllib.request.Request(url, headers={'Authorization': f'Bearer {token}'})
import json
with urllib.request.urlopen(req, timeout=10) as r:
    doc = json.loads(r.read())
    p = doc['photos'][0]
    print(f"Testing photo id={p['id']} filename={p['filename']} media_type={p['media_type']}")

# Probe /api/photo (full bytes)
url = f'http://{addr}:{port}/api/photo/{p["id"]}'
req = urllib.request.Request(url, headers={'Authorization': f'Bearer {token}'})
try:
    with urllib.request.urlopen(req, timeout=30) as r:
        ct = r.headers.get('Content-Type')
        cl = r.headers.get('Content-Length')
        cr = r.headers.get('Content-Range')
        print(f"/api/photo: HTTP {r.status}")
        print(f"  Content-Type: {ct}")
        print(f"  Content-Length: {cl}")
        print(f"  Accept-Ranges: {r.headers.get('Accept-Ranges')}")
        data = r.read(64)
        first16 = ' '.join(f'{b:02X}' for b in data[:16])
        print(f"  First 16 bytes: {first16}")
        # MP4/MOV files start with 'ftyp' atom at offset 4
        if len(data) >= 8 and data[4:8] == b'ftyp':
            brand = data[8:12].decode('ascii', errors='replace')
            print(f"  -> ISO base media (MP4/MOV), brand='{brand}'")
        else:
            print(f"  -> unknown / corrupt header")
except urllib.error.HTTPError as e:
    body = e.read().decode('utf-8', errors='replace')[:300]
    print(f"HTTPError {e.code}: {body}")
except Exception as e:
    print(f"ERROR: {e}")
