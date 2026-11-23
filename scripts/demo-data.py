#!/usr/bin/env python3
"""Creates a throwaway demo setup for trying docgait by hand (used by `make demo`).

<dir>/drives/CopyA|B|C  three overlapping copies of the same files, with:
  - files unique to one copy, duplicates under other names ("receipt (2).pdf")
  - the same path edited differently (Notes/plan.md: a conflict)
  - a code project with .gitignore'd build output and node_modules (excluded)
  - an Obsidian vault with a link between notes
<dir>/Library           an empty folder to use as the library
<dir>/data, <dir>/trash  app data and trash, kept away from your real ones
"""

import os
import shutil
import sys
import time

DAY = 86400


def write(path, body, days_ago=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(body)
    if days_ago is not None:
        t = time.time() - days_ago * DAY
        os.utime(path, (t, t))


def main():
    base = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/demo")
    shutil.rmtree(base, ignore_errors=True)
    drives = os.path.join(base, "drives")
    for name in ["CopyA", "CopyB", "CopyC"]:
        r = os.path.join(drives, name)
        write(f"{r}/Taxes/2023/W2.pdf", "W-2 wage statement 2023", 700)
        write(f"{r}/Photos/2019/beach.jpg", "beach photo bytes " * 200, 2000)
        write(f"{r}/Notes/todo.md", "- buy milk\n- call mom\n", 300)
        write(f"{r}/Projects/app/.gitignore", "build/\n")
        write(f"{r}/Projects/app/src/main.rs", 'fn main() {\n    println!("hi");\n}\n')
        write(f"{r}/Projects/app/build/app.bin", "compiled junk " * 100)
        write(f"{r}/Projects/app/node_modules/left-pad/index.js", "module.exports = 1;\n")
        write(f"{r}/.DS_Store", "junk")
    write(f"{drives}/CopyA/Letters/to-grandma.txt", "Dear Grandma,\n", 900)
    write(f"{drives}/CopyB/Taxes/2023/receipt.pdf", "receipt for the new laptop", 650)
    write(f"{drives}/CopyB/Taxes/2023/receipt (2).pdf", "receipt for the new laptop", 650)
    write(f"{drives}/CopyB/Photos/2019/beach copy.jpg", "beach photo bytes " * 200, 2000)
    # Same path, different content, dates far apart: CopyB's is newest and is suggested.
    write(f"{drives}/CopyA/Notes/plan.md", "# Plan\n\n- step one\n- step two\n", 120)
    write(f"{drives}/CopyB/Notes/plan.md", "# Plan\n\n- step one\n- step 2 (revised)\n- step three\n", 10)
    write(f"{drives}/CopyC/Notes/plan.md", "# Plan\n\n- step one\n- step two\n- extra idea\n", 60)
    write(f"{drives}/CopyC/Vault/.obsidian/app.json", "{}\n")
    write(f"{drives}/CopyC/Vault/Home.md", "Start here. See [[Ideas]].\n", 30)
    write(f"{drives}/CopyC/Vault/Ideas.md", "- an idea\n", 30)
    os.makedirs(os.path.join(base, "Library"))
    print(base)


if __name__ == "__main__":
    main()
