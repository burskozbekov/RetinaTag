# Reverse the partial Phase-2 moves: put every moved file back at its original
# path so it matches the pre-Phase2 DB we restore next. Uses the manifest (exact
# new paths for the 50 committed moves) PLUS the plan (target_dir/base for the
# errored moves whose file was renamed but DB not updated).
import os, csv

PLAN = r"D:/Fotograflar/RetinaTag/retina.db.misfiled-plan.tsv"
MAN  = r"D:/Fotograflar/RetinaTag/retina.db.refile-plan-manifest.csv"
ROOT = r"D:\Fotograflar"
MON  = ["", "January","February","March","April","May","June","July",
        "August","September","October","November","December"]

def ym(s):
    try:
        y=int(s[0:4]); m=int(s[5:7])
        if 1990<=y<=2099 and 1<=m<=12: return (y,m)
    except Exception: pass
    return None
def folder_ym(folder):
    mm=os.path.basename(folder); yy=os.path.basename(os.path.dirname(folder))
    try:
        y=int(yy); m=int(mm[:2])
        if 1990<=y<=2099 and 1<=m<=12: return (y,m)
    except Exception: pass
    return None

# old_path -> new_path (where the file is NOW)
moved = {}
# 1) exact, from manifest (committed 50)
if os.path.exists(MAN):
    with open(MAN, encoding="utf-8") as f:
        for r in csv.DictReader(f):
            moved[r["old_path"]] = r["new_path"]
# 2) inferred, from plan (errored ones: file at target_dir/base)
with open(PLAN, encoding="utf-8") as f:
    rd=csv.reader(f, delimiter="\t"); next(rd, None)
    for r in rd:
        if len(r)<5: continue
        old=r[1]; folder=r[2]; emb=r[4].strip()
        if not emb: continue
        eym=ym(emb); fym=folder_ym(folder)
        if not eym or eym==fym: continue
        if old in moved: continue
        tgt=os.path.join(ROOT, f"{eym[0]:04d}", f"{eym[1]:02d}-{MON[eym[1]]}", os.path.basename(old))
        moved[old]=tgt

back=already=miss=err=0
for old,new in moved.items():
    if os.path.exists(old):
        already+=1
        continue
    if not os.path.exists(new):
        miss+=1
        continue
    try:
        os.makedirs(os.path.dirname(old), exist_ok=True)
        os.rename(new, old); back+=1
    except Exception as e:
        print("  ERR", new, "->", old, ":", e); err+=1
print(f"candidates: {len(moved)}  moved BACK: {back}  already-at-old: {already}  new-missing: {miss}  errors: {err}")
