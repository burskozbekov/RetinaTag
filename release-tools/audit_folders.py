# Read-only audit: every photo whose physical YYYY\MM-Month folder disagrees
# with its date_taken. Uses os.path (no backslash literals) to dodge shell
# escaping. Prints the full picture so we know the true scale before re-filing.
import sqlite3, os

DB = r"D:/Fotograflar/RetinaTag/retina.db"

def folder_ym(folder):
    if not folder:
        return None
    mm_month = os.path.basename(folder)            # "09-September"
    yyyy = os.path.basename(os.path.dirname(folder))  # "2020"
    if len(yyyy) == 4 and yyyy.isdigit() and len(mm_month) >= 2 and mm_month[:2].isdigit():
        y, m = int(yyyy), int(mm_month[:2])
        if 1990 <= y <= 2099 and 1 <= m <= 12:
            return (y, m)
    return None

con = sqlite3.connect(DB, timeout=30)
con.execute("PRAGMA query_only=ON")
c = con.cursor()
rows = c.execute("SELECT id, folder, date_taken FROM photos").fetchall()
print("total photos:", len(rows))

in_date = nondate = correct = mism = mism_null = 0
mism_by_folder_year = {}
mism_by_delta = {}   # |folder_year - date_year| buckets
examples = []
nondate_examples = {}

for pid, folder, dt in rows:
    fym = folder_ym(folder)
    if fym is None:
        nondate += 1
        key = os.path.basename(folder) if folder else "(none)"
        nondate_examples[key] = nondate_examples.get(key, 0) + 1
        continue
    in_date += 1
    if not dt:
        mism_null += 1
        continue
    dy, dm = int(dt[:4]), int(dt[5:7])
    if (dy, dm) == fym:
        correct += 1
    else:
        mism += 1
        mism_by_folder_year[fym[0]] = mism_by_folder_year.get(fym[0], 0) + 1
        d = abs(fym[0] - dy)
        mism_by_delta[d] = mism_by_delta.get(d, 0) + 1
        if len(examples) < 20:
            examples.append((folder.split("Fotograflar")[-1], dt[:10]))

print(f"in YYYY\\MM date-folders : {in_date}")
print(f"non-date folders        : {nondate}")
print(f"  folder == date (good) : {correct}  ({100*correct/max(in_date,1):.1f}% of date-folders)")
print(f"  MISMATCH (misfiled)   : {mism}")
print(f"  in date-folder, NULL  : {mism_null}")
print()
print("mismatches by FOLDER year (top 20):")
for y, n in sorted(mism_by_folder_year.items(), key=lambda x: -x[1])[:20]:
    print(f"     folder {y}: {n}")
print()
print("mismatch year-distance (0=same yr diff month, 1=+/-1yr, big=way off):")
for d, n in sorted(mism_by_delta.items()):
    print(f"     |delta|={d}: {n}")
print()
print("sample mismatches (folder tail, real date):")
for e in examples:
    print("    ", e)
print()
print("non-date folder names (top 15):")
for k, n in sorted(nondate_examples.items(), key=lambda x: -x[1])[:15]:
    print(f"     {n:6d}  {k!r}")
con.close()
