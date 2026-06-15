# Reverse the refile file moves (new_path -> old_path) using the manifest, so
# the physical layout matches the clean pre-refile DB backup we restore next.
import os, csv

MAN = r"D:/Fotograflar/RetinaTag/retina.db.refile-manifest.csv"
back = same = miss_both = err = newdir_busy = 0
with open(MAN, encoding="utf-8") as f:
    rows = list(csv.DictReader(f))
print("manifest moves:", len(rows))
for m in rows:
    old = m["old_path"]; new = m["new_path"]
    if os.path.exists(old) and not os.path.exists(new):
        same += 1                      # already at old (move never happened / already reverted)
        continue
    if not os.path.exists(new):
        if not os.path.exists(old):
            miss_both += 1             # neither exists — investigate later
        continue
    # new exists. move it back to old.
    if os.path.exists(old):
        newdir_busy += 1               # both exist — don't overwrite; leave new in place
        continue
    try:
        os.makedirs(os.path.dirname(old), exist_ok=True)
        os.rename(new, old)
        back += 1
    except Exception as e:
        print("  ERR", new, "->", old, ":", e); err += 1
print(f"moved BACK: {back}  already-at-old: {same}  both-exist(skip): {newdir_busy}  missing-both: {miss_both}  errors: {err}")
