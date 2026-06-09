import sqlite3, urllib.request, json, sys, os

candidates = [
    r'D:\Fotograflar\RetinaTag\retina.db',
    os.path.expandvars(r'%USERPROFILE%\Pictures\RetinaTag\retina.db'),
    os.path.expandvars(r'%APPDATA%\com.retinatag.app\retina.db'),
]
db = next((p for p in candidates if os.path.exists(p)), None)
if not db:
    print('NO DB'); sys.exit(1)
print(f'DB: {db}')

conn = sqlite3.connect(db)
rows = conn.execute(
    "SELECT peer_name, addr, port, token FROM lan_peer_tokens"
).fetchall()
if not rows:
    print('No paired peers')
    sys.exit(1)
for peer_name, addr, port, token in rows:
    print(f'\n=== Peer: {peer_name} {addr}:{port} ===')
    print(f'Token: {token[:8]}...{token[-8:]} (len={len(token)})')

    # 1) /api/photos?vault_only=true
    url = f'http://{addr}:{port}/api/photos?vault_only=true&offset=0&limit=3'
    req = urllib.request.Request(url, headers={'Authorization': f'Bearer {token}'})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            body = r.read().decode('utf-8', errors='replace')
            print(f'/api/photos: HTTP {r.status}')
            try:
                doc = json.loads(body)
                photos = doc.get('photos', [])
                print(f'  total={doc.get("total")} returned={len(photos)}')
                for ph in photos[:3]:
                    print(f'  id={ph.get("id")} filename={ph.get("filename")} media_type={ph.get("media_type")}')
                first_id = photos[0]['id'] if photos else None
            except Exception as e:
                print(f'  JSON parse failed: {e}')
                print(f'  body (first 400): {body[:400]}')
                first_id = None
    except urllib.error.HTTPError as e:
        print(f'/api/photos: HTTPError {e.code} {e.reason}')
        first_id = None
    except Exception as e:
        print(f'/api/photos: ERROR {e}')
        first_id = None

    if not first_id:
        print('  No photo id available, skipping thumb test')
        continue

    # 2) /api/thumb/{id}
    url = f'http://{addr}:{port}/api/thumb/{first_id}'
    req = urllib.request.Request(url, headers={'Authorization': f'Bearer {token}'})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            data = r.read()
            print(f'/api/thumb/{first_id}: HTTP {r.status}')
            print(f'  Content-Type: {r.headers.get("Content-Type")}')
            print(f'  Content-Length: {len(data)}')
            first16 = ' '.join(f'{b:02X}' for b in data[:16])
            print(f'  First 16 bytes: {first16}')
            # JPEG starts with FF D8, PNG with 89 50 4E 47, JSON with {
            if data.startswith(b'\xff\xd8'):
                print('  -> JPEG ✓')
            elif data.startswith(b'\x89PNG'):
                print('  -> PNG ✓')
            elif data.startswith(b'{'):
                print(f'  -> JSON (suspicious): {data[:200].decode("utf-8", errors="replace")}')
            else:
                print(f'  -> unknown format')
    except urllib.error.HTTPError as e:
        body = e.read().decode('utf-8', errors='replace')[:300] if hasattr(e, 'read') else ''
        print(f'/api/thumb/{first_id}: HTTPError {e.code} {e.reason}  body={body}')
    except Exception as e:
        print(f'/api/thumb/{first_id}: ERROR {e}')
