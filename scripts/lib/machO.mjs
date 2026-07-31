// Minimal Mach-O / universal-binary magic-number detection.
//
// codesign-managed-runtime.mjs uses this to find every signable binary
// under the staged llama.cpp managed-runtime tree by content, not by a
// hardcoded filename/extension allowlist. A future llama.cpp release can
// rename, add, or restructure its shared libraries and executables at any
// time; if signing only recognized "*.dylib" and "llama-server" by name, a
// renamed or newly added binary would be staged as an inert resource,
// skipped by signing, and fail Apple notarization again - exactly the bug
// this file exists to prevent from recurring.

import { closeSync, openSync, readSync } from "node:fs";

const MACHO_MAGICS = new Set([
  0xfeedface, // MH_MAGIC     - 32-bit Mach-O
  0xcefaedfe, // MH_CIGAM     - 32-bit Mach-O, byte-swapped
  0xfeedfacf, // MH_MAGIC_64  - 64-bit Mach-O
  0xcffaedfe, // MH_CIGAM_64  - 64-bit Mach-O, byte-swapped
  0xcafebabe, // FAT_MAGIC    - universal binary
  0xbebafeca, // FAT_CIGAM    - universal binary, byte-swapped
]);

export function isMachOFile(path) {
  const fd = openSync(path, "r");
  try {
    const header = Buffer.alloc(4);
    const bytesRead = readSync(fd, header, 0, 4, 0);
    if (bytesRead < 4) return false;
    return MACHO_MAGICS.has(header.readUInt32BE(0));
  } finally {
    closeSync(fd);
  }
}
