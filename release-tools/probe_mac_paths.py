import sqlite3, urllib.request, json
db = r'D:\Fotograflar\RetinaTag\retina.db'
conn = sqlite3.connect(db)
token, addr, port = conn.execute("SELECT token, addr, port FROM lan_peer_tokens LIMIT 1").fetchone()

url = f'http://{addr}:{port}/api/photos?vault_only=true&offset=0&limit=5'
req = urllib.request.Request(url, headers={'Authorization': f'Bearer {token}'})
with urllib.request.urlopen(req, timeout=10) as r:
    doc = json.loads(r.read())
print(f"Mac vault items (first 5 of {doc.get('total')}):")
for p in doc['photos'][:5]:
    print(f"  id={p['id']}")
    print(f"     filename = {p.get('filename')}")
    print(f"     path     = {p.get('path')}")
    print(f"     media    = {p.get('media_type')}")
    print(f"     extras   = {[k for k in p if k not in ('id','filename','path','media_type','status','tags','tag_count','rating','favorite','provider_used','date_taken','duration_secs','width','height')]}")
    print()
