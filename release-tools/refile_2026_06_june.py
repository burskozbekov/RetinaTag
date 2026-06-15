# One-shot, reversible re-file of the photos that the mtime-bucketing bug
# dumped into D:\Fotograflar\2026\06-June\. Their date_taken was already
# corrected (v1.5.414); this moves the physical files to the folder matching
# their real capture date and updates the DB path/folder. Same-drive rename
# (atomic, no copy), no overwrite (collisions get _N), full manifest written
# for reversal. App MUST be closed before running.
import sqlite3, os, csv, sys, time

DB = r"D:/Fotograflar/RetinaTag/retina.db"
ROOT = r"D:\Fotograflar"
MON = ["", "January","February","March","April","May","June","July",
       "August","September","October","November","December"]
MANIFEST = r"C:\Users\dede_\Desktop\RetinaTag\.claude\worktrees\beautiful-blackwell\release-tools\refile_manifest.csv"

def main():
    con = sqlite3.connect(DB, timeout=60)
    c = con.cursor()
    rows = c.execute(
        r"SELECT id,path,date_taken FROM photos WHERE path LIKE 'D:\Fotograflar\2026\06-June\%'"
    ).fetchall()
    print(f"candidates in 2026\\06-June: {len(rows)}")

    plan = []  # (id, old, new, folder)
    skip_null = skip_june = skip_missing = 0
    for pid, old, dt in rows:
        if not os.path.exists(old): skip_missing += 1; continue
        if not dt: skip_null += 1; continue
        y, m = int(dt[:4]), int(dt[5:7])
        if y == 2026 and m == 6: skip_june += 1; continue
        tgt_dir = os.path.join(ROOT, f"{y:04d}", f"{m:02d}-{MON[m]}")
        os.makedirs(tgt_dir, exist_ok=True)
        base = os.path.basename(old)
        new = os.path.join(tgt_dir, base)
        if os.path.exists(new):  # never overwrite — append _N
            stem, ext = os.path.splitext(base)
            n = 1
            while os.path.exists(os.path.join(tgt_dir, f"{stem}_{n}{ext}")):
                n += 1
            new = os.path.join(tgt_dir, f"{stem}_{n}{ext}")
        plan.append((pid, old, new, tgt_dir))

    print(f"will move: {len(plan)}  skip(june)={skip_june} skip(null)={skip_null} skip(missing)={skip_missing}")

    # Write reversal manifest FIRST.
    with open(MANIFEST, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(["id", "old_path", "new_path"])
        for pid, old, new, _ in plan:
            w.writerow([pid, old, new])
    print("manifest:", MANIFEST)

    moved = errs = 0
    con.execute("BEGIN")
    for pid, old, new, folder in plan:
        try:
            os.rename(old, new)  # same drive → atomic
            c.execute("UPDATE photos SET path=?, folder=? WHERE id=?", (new, folder, pid))
            moved += 1
        except Exception as e:
            print("  ERR", pid, old, "->", new, ":", e)
            errs += 1
    con.commit()
    print(f"MOVED {moved}, errors {errs}")

    # verify
    left = c.execute(r"SELECT COUNT(*) FROM photos WHERE path LIKE 'D:\Fotograflar\2026\06-June\%'").fetchone()[0]
    print("remaining in 2026\\06-June (DB):", left, "(expect ~1 genuine June photo)")
    con.close()

if __name__ == "__main__":
    main()
