import sqlite3, urllib.request, json
db = r'D:\Fotograflar\RetinaTag\retina.db'
conn = sqlite3.connect(db)
token, addr, port = conn.execute("SELECT token, addr, port FROM lan_peer_tokens LIMIT 1").fetchone()

# /api/photos full dump
url = f'http://{addr}:{port}/api/photos?vault_only=true&offset=0&limit=2'
req = urllib.request.Request(url, headers={'Authorization': f'Bearer {token}'})
with urllib.request.urlopen(req, timeout=10) as r:
    raw = r.read().decode('utf-8', errors='replace')
print("Raw response body (first 2 KB):")
print(raw[:2048])
print()
print("---")
try:
    doc = json.loads(raw)
    if doc.get('photos'):
        print("First photo full JSON:")
        print(json.dumps(doc['photos'][0], indent=2, ensure_ascii=False))
        print()
        print("Top-level response keys:", list(doc.keys()))
except Exception as e:
    print(f"parse error: {e}")

# Probe other plausible endpoints Mac might already expose
print("\n=== Trying /api/vault/folders ===")
for url in [
    f'http://{addr}:{port}/api/vault/folders',
    f'http://{addr}:{port}/api/folders',
    f'http://{addr}:{port}/api/vault/list',
]:
    req = urllib.request.Request(url, headers={'Authorization': f'Bearer {token}'})
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            body = r.read().decode('utf-8', errors='replace')[:400]
            print(f"  {url} → {r.status} {body}")
    except urllib.error.HTTPError as e:
        print(f"  {url} → HTTP {e.code} {e.reason}")
    except Exception as e:
        print(f"  {url} → {type(e).__name__}: {e}")
