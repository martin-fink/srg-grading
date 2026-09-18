"""Copy a verified immutable seed into a fresh, private emptyDir."""
from pathlib import Path
import shutil
import stat
import sys


def copy(source, target):
    for entry in source.iterdir():
        output = target / entry.name
        mode = entry.stat(follow_symlinks=False).st_mode
        if stat.S_ISDIR(mode):
            output.mkdir(mode=0o700)
            copy(entry, output)
            output.chmod(0o755)
        elif stat.S_ISREG(mode):
            shutil.copyfile(entry, output, follow_symlinks=False)
            output.chmod(0o755 if mode & 0o111 else 0o644)
        else:
            raise ValueError("cache contains a non-regular entry")


# The volume root can be owned by root with a writable fsGroup. Never chmod it.
copy(Path(sys.argv[1]), Path(sys.argv[2]))
