# Phase 2 of the safe folder re-file. Reads the read-only plan TSV produced by
# the app's export_misfiled_plan command and moves each photo into the folder
# matching its REAL embedded capture date (column 5), updating the DB.
# MUST run with RetinaTag CLOSED (no scan/watcher) so there is zero race.
# Same-drive os.rename (atomic, no copy), never overwrites (collisions get _N),
# leaves rows with no embedded date untouched. Writes a reversal manifest.
import sqlite3, os, csv, sys

DB   = r"D:/Fotograflar/RetinaTag/retina.db"
PLAN = r"D:/Fotograflar/RetinaTag/retina.db.misfiled-plan.tsv"
ROOT = r"D:\Fotograflar"
MAN  = r"D:/Fotograflar/RetinaTag/retina.db.refile-plan-manifest.csv"
MON  = ["", "January","February","March","April","May","June","July",
        "August","September","October","November","December"]

def ym(s):
    try:
        y = int(s[0:4]); m = int(s[5:7])
        if 1990 <= y <= 2099 and 1 <= m <= 12: return (y, m)
    except Exception: pass
    return None

def folder_ym(folder):
    mm = os.path.basename(folder); yyyy = os.path.basename(os.path.dirname(folder))
    try:
        y = int(yyyy); m = int(mm[:2])
        if 1990 <= y <= 2099 and 1 <= m <= 12: return (y, m)
    except Exception: pass
    return None

def main(apply=False):
    if not os.path.exists(PLAN):
        print("PLAN MISSING:", PLAN); return
    rows = []
    with open(PLAN, encoding="utf-8") as f:
        rd = csv.reader(f, delimiter="\t")
        header = next(rd, None)
        for r in rd:
            if len(r) < 5: continue
            rows.append(r)   # id, path, folder, date_taken, embedded
    print("plan rows (mismatches):", len(rows))

    moves = []      # (id, old, newdir, newdate)
    datefix = []    # (id, newdate)
    no_emb = 0
    for r in rows:
        pid = int(r[0]); path = r[1]; folder = r[2]; emb = r[4].strip()
        if not emb:
            no_emb += 1; continue
        eym = ym(emb)
        if not eym:
            no_emb += 1; continue
        fym = folder_ym(folder)
        if fym == eym:
            datefix.append((pid, emb))           # right folder, wrong date_taken
            continue
        tgt_dir = os.path.join(ROOT, f"{eym[0]:04d}", f"{eym[1]:02d}-{MON[eym[1]]}")
        moves.append((pid, path, tgt_dir, emb))
    print(f"would MOVE: {len(moves)}   date-only-fix: {len(datefix)}   no-embedded(skip): {no_emb}")
    # distribution of moves by target year
    byyear = {}
    for _,_,td,_ in moves:
        y = os.path.basename(os.path.dirname(td))
        byyear[y] = byyear.get(y,0)+1
    print("moves by target year:", dict(sorted(byyear.items())))
    for r in moves[:6]:
        print("   ", os.path.basename(r[1]), "->", r[2].split("Fotograflar")[-1], "(", r[3][:10], ")")

    if not apply:
        print("\nDRY-RUN only. Re-run with 'apply' to execute.")
        return

    con = sqlite3.connect(DB, timeout=60); c = con.cursor()
    manf = open(MAN, "w", newline="", encoding="utf-8"); mw = csv.writer(manf)
    mw.writerow(["id","old_path","new_path"])
    moved = fixed = err = 0
    con.execute("BEGIN")
    for pid, old, tgt_dir, emb in moves:
        try:
            if not os.path.exists(old):
                err += 1; continue
            os.makedirs(tgt_dir, exist_ok=True)
            base = os.path.basename(old)
            new = os.path.join(tgt_dir, base)
            if os.path.exists(new):
                stem, ext = os.path.splitext(base); n = 1
                while os.path.exists(os.path.join(tgt_dir, f"{stem}_{n}{ext}")): n += 1
                new = os.path.join(tgt_dir, f"{stem}_{n}{ext}")
            os.rename(old, new)
            c.execute("UPDATE photos SET path=?, folder=?, date_taken=? WHERE id=?",
                      (new, tgt_dir, emb, pid))
            mw.writerow([pid, old, new]); moved += 1
        except Exception as e:
            print("  ERR move", pid, e); err += 1
    for pid, emb in datefix:
        try:
            c.execute("UPDATE photos SET date_taken=? WHERE id=?", (emb, pid)); fixed += 1
        except Exception as e:
            print("  ERR datefix", pid, e); err += 1
    con.commit(); con.close(); manf.close()
    print(f"APPLIED: moved={moved} date_fixed={fixed} errors={err}")
    print("manifest:", MAN)

if __name__ == "__main__":
    main(apply=(len(sys.argv) > 1 and sys.argv[1] == "apply"))
