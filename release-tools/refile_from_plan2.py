# Phase 2 (v2) — safe, collision-correct folder re-file from the read-only plan.
# RetinaTag MUST be CLOSED. For each misfiled photo with a real embedded date:
#   target = <lib>\YYYY\MM-Month\<name>
#   - if a REAL file already occupies target on disk -> append _N (keep both)
#   - else if a DEAD DB row (file missing) occupies target -> delete that dead
#     row (its file is already gone) and use the clean name
#   - move the file (same-drive os.rename) + update path/folder/date_taken
# No overwrites. Photos with no embedded date are left untouched. Writes a
# move manifest; the full DB backup (taken before running) is the master undo.
import sqlite3, os, csv, sys

DB   = r"D:/Fotograflar/RetinaTag/retina.db"
PLAN = r"D:/Fotograflar/RetinaTag/retina.db.misfiled-plan.tsv"
ROOT = r"D:\Fotograflar"
MAN  = r"D:/Fotograflar/RetinaTag/retina.db.refile2-manifest.csv"
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

def load_moves():
    out=[]
    with open(PLAN, encoding="utf-8") as f:
        rd=csv.reader(f, delimiter="\t"); next(rd, None)
        for r in rd:
            if len(r)<5: continue
            pid=int(r[0]); old=r[1]; folder=r[2]; emb=r[4].strip()
            if not emb: continue
            e=ym(emb);
            if not e or e==folder_ym(folder): continue
            tgt_dir=os.path.join(ROOT, f"{e[0]:04d}", f"{e[1]:02d}-{MON[e[1]]}")
            out.append((pid, old, tgt_dir, emb))
    return out

def main(apply=False):
    if not os.path.exists(PLAN):
        print("PLAN MISSING:", PLAN); return
    moves=load_moves()
    print("move candidates:", len(moves))
    if not apply:
        print("DRY-RUN. add 'apply' to execute.");
    con=sqlite3.connect(DB, timeout=60); c=con.cursor()
    dbpaths=set(p for (p,) in c.execute("SELECT path FROM photos"))
    moved=ghost_del=name_bumped=err=0
    manf=None
    if apply:
        manf=open(MAN,"w",newline="",encoding="utf-8"); mw=csv.writer(manf)
        mw.writerow(["id","old_path","new_path","ghost_deleted_path"])
        con.execute("BEGIN")
    for pid, old, tgt_dir, emb in moves:
        base=os.path.basename(old); stem,ext=os.path.splitext(base)
        cand=os.path.join(tgt_dir, base)
        ghost=None
        # 1) real file on disk -> _N
        if os.path.exists(cand):
            n=1
            while os.path.exists(os.path.join(tgt_dir,f"{stem}_{n}{ext}")) or (os.path.join(tgt_dir,f"{stem}_{n}{ext}") in dbpaths):
                n+=1
            cand=os.path.join(tgt_dir,f"{stem}_{n}{ext}"); name_bumped+=1
        else:
            # disk-free; if a DB row holds this path it's a DEAD/ghost row
            if cand in dbpaths:
                ghost=cand
        if not apply:
            continue
        try:
            os.makedirs(tgt_dir, exist_ok=True)
            if not os.path.exists(old):
                err+=1; continue
            os.rename(old, cand)
            if ghost is not None:
                c.execute("DELETE FROM photos WHERE path=?", (ghost,)); ghost_del+=1
                dbpaths.discard(ghost)
            c.execute("UPDATE photos SET path=?, folder=?, date_taken=? WHERE id=?",
                      (cand, tgt_dir, emb, pid))
            dbpaths.discard(old); dbpaths.add(cand)
            mw.writerow([pid, old, cand, ghost or ""]); moved+=1
        except Exception as e:
            print("  ERR", pid, base, "->", cand, ":", e); err+=1
    if apply:
        con.commit(); manf.close()
        print(f"APPLIED: moved={moved}  dead-rows-removed={ghost_del}  name-bumped(_N)={name_bumped}  errors={err}")
        print("manifest:", MAN)
    else:
        # dry-run accounting
        gh=sum(1 for (pid,old,td,emb) in moves if (not os.path.exists(os.path.join(td,os.path.basename(old)))) and (os.path.join(td,os.path.basename(old)) in dbpaths))
        rl=sum(1 for (pid,old,td,emb) in moves if os.path.exists(os.path.join(td,os.path.basename(old))))
        print(f"  would: move={len(moves)}  dead-rows-to-remove={gh}  real-file-collisions(_N)={rl}  clean={len(moves)-gh-rl}")
    con.close()

if __name__=="__main__":
    main(apply=(len(sys.argv)>1 and sys.argv[1]=="apply"))
