#!/usr/bin/env bash
# Build the independent reference extractor that the strongest tests compare against.
#
# This compiles Macrium's own contrib/extract-to-img demo from the linux-contrib-tidy
# branch. It is a separate implementation by different authors, so a bug in our Rust
# reader and a bug in this extractor do not cancel each other out. That independence is
# the whole point: validating our writer with our own reader proves nothing.
#
# Two changes are made to the upstream sources. Both are recorded here rather than
# hidden, because an oracle you edited is only trustworthy when the edit is visible.
#
#   1. FindBackupFiles follows the absolute Windows paths recorded in file_history, for
#      example "D:\Users\someone\Backup-Files\SET-00-00.mrimg". Those paths do not exist
#      on this machine, so chain following fails at once. The patch resolves each
#      recorded name against the directory of the file passed on the command line. It
#      changes how a member is located, never how the chain is resolved.
#   2. main_linux.cpp calls losetup and waits for a keypress. A driver that writes a
#      plain file replaces it.
#
# Upstream limits that still apply: this extractor handles neither compression nor
# encryption, so it works only on the uncompressed test corpus.
#
# Usage:
#   ./scripts/build-refextract.sh              # builds /tmp/refextract
#   /tmp/refextract BACKUP.mrimg OUT.img
#
# The tests read the path from REFEXTRACT, and fall back to /tmp/refextract. They skip
# when the oracle is absent.

set -euo pipefail

CORPUS=${CORPUS:-/tmp/mrimgx-corpus}
SRC="$CORPUS/contrib/extract-to-img"
OUT=${OUT:-/tmp/refextract}

if [ ! -f "$SRC/libs/restore/backup_set.cpp" ]; then
    "$(dirname "$0")/fetch-corpus.sh"
fi

# Patch 1: resolve set members against the local directory.
python3 - "$SRC/libs/restore/backup_set.cpp" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1]); t = p.read_text()
if "ResolveMember" in t:
    print("  backup_set.cpp already patched")
    raise SystemExit
helper = r'''
// PATCH for use as a test oracle on Linux. See scripts/build-refextract.sh.
static std::string g_backupDirectory;
void SetBackupDirectory(const std::string& directory) { g_backupDirectory = directory; }
static std::string ResolveMember(const std::string& recorded)
{
    size_t cut = recorded.find_last_of("/\\");
    std::string base = (cut == std::string::npos) ? recorded : recorded.substr(cut + 1);
    if (g_backupDirectory.empty()) { return base; }
    return g_backupDirectory + "/" + base;
}

'''
t = t.replace('void FindBackupFiles(', helper + 'void FindBackupFiles(', 1)
old = ('        backupSet.filePaths.insert(backupSet.filePaths.begin(), fileHistory.file_name);\n'
       '        file_structs::File_Layout fileLayout;\n'
       '        readBackupFileLayout(fileLayout, fileHistory.file_name);')
new = ('        const std::string resolved = ResolveMember(fileHistory.file_name);\n'
       '        backupSet.filePaths.insert(backupSet.filePaths.begin(), resolved);\n'
       '        file_structs::File_Layout fileLayout;\n'
       '        readBackupFileLayout(fileLayout, resolved);')
assert old in t, "upstream FindBackupFiles body changed; re-read it before patching"
t = t.replace(old, new, 1)
p.write_text(t)
print("  patched backup_set.cpp")
PY

# Patch 2: a driver that writes a file instead of attaching a loop device.
cat > /tmp/refextract-main.cpp <<'CPP'
#include <cstdio>
#include <string>
#include <fstream>
#include <filesystem>
#include "libs/img_handler/img_handler.h"
#include "libs/restore/restore.h"
#include "libs/file_handler/file_handler.h"

void SetBackupDirectory(const std::string& directory);

int main(int argc, char** argv) {
    if (argc != 3) { fprintf(stderr, "usage: refextract BACKUP OUT.img\n"); return 2; }
    std::filesystem::path in = std::filesystem::absolute(argv[1]);
    SetBackupDirectory(in.parent_path().string());
    file_structs::File_Layout layout;
    readBackupFileLayout(layout, in.string());
    uint64_t size = layout.disks[0]._geometry.disk_size;
    { std::ofstream f(argv[2], std::ios::binary | std::ios::trunc);
      f.seekp(size - 1); f.put('\0'); }
    restoreDisk(in.string(), argv[2], layout, 0);
    fprintf(stderr, "wrote %s (%llu bytes)\n", argv[2], (unsigned long long)size);
    return 0;
}
CPP

g++ -std=c++17 -O2 -o "$OUT" /tmp/refextract-main.cpp \
    "$SRC/libs/img_handler/img_handler.cpp" \
    "$SRC/libs/restore/restore.cpp" \
    "$SRC/libs/restore/backup_set.cpp" \
    "$SRC/libs/file_handler/file_handler.cpp" \
    -I"$SRC" -I"$SRC/dependencies/include"

echo "built $OUT"
