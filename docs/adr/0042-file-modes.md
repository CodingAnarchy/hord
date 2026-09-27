# ADR 0042: File modes in trees

- **Status:** accepted
- **Date:** 2026-09-27
- **Spec:** §3.2 (`Tree`), §6.1 (materialization), §9 (round trip)
- **Blocks:** M6 (hord-first development, ADR 0040)

## Problem

A git tree records each file's mode: regular (`100644`), executable (`100755`), symlink (`120000`), or submodule (`160000`). A hord `TreeEntry` has no mode. To keep §9's round trip, the git import wrapped every non-regular file in a private hord-git object (`GitLeaf { mode, blob }`) and put that object's id in the `TreeEntry::Blob`. Nothing outside hord-git knew about it. Every other reader decoded the entry as a `Blob` and failed. So `hord ws new` failed on hord's own repository (`.claude/skills` is a symlink, and the scripts are executable) with `missing field 'bytes'`. A workspace also could not read a symlink or an exec bit back, and a symlink in a checkout was an error.

## Options

1. **Add a mode to `TreeEntry`** (a field, or a variant per mode). This is the most explicit option, but it changes the encoding of every tree. Every imported repository would get new object ids and have to be imported again.
2. **Make the leaf a core object.** Move the wrapper into hord-core with the same fields, so the same encoding and the same ids. Every reader of a file entry resolves it. A regular file stays a plain `Blob`.
3. **Drop modes.** Every file becomes regular. This breaks §9's round trip for any repository with a symlink or an executable.

## Decision

Option 2. A `TreeEntry::Blob` names either a `Blob` (a regular file) or a `ModedBlob { mode, blob }`, where `mode` is git's octal and `blob` names the `Blob` with the bytes. For a symlink the bytes are the link target, and for a gitlink they are the commit's hex SHA. The entry's id covers the mode, so a mode-only edit is a change of the file, recorded as an `Op::Blob`. A symlink or gitlink is never parsed by an adapter.

How workspaces handle modes:

- **Checkout.** An executable gets its exec bits and a symlink becomes a real symlink on Unix. A gitlink becomes an empty directory, as git leaves an uninitialized submodule.
- **Windows.** Without symlinks or exec bits on disk (git's `core.symlinks` and `core.fileMode` off), a symlink is a regular file that holds its target. Reading a tracked file back keeps its base mode, and a new file is regular.
- **Read back.** A `Directory` walk treats a symlink as a file whose bytes are its target and never follows it. The stat index records the exec bit, since `chmod` does not change the mtime. Only fifos, sockets, and other special files stay unsupported.
- **In memory.** A file written through the API keeps its base mode.
- **Rebase.** A merged file takes the side that changed the mode, or theirs when both changed it.

## Consequences

- Existing imports keep their ids. hord's own served repository works without being imported again.
- Every reader of file bytes must resolve a `ModedBlob`: hord-txn's `blob_bytes`, git export, the sync object cache, and `push_change`, which sends the wrapped blob too. A new reader that decodes a `Blob` directly is a bug.
- A mode is git's octal string, and only the four modes above are known. Supporting another mode, or moving modes into `TreeEntry`, needs a new ADR.
